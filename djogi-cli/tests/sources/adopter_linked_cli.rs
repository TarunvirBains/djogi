// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

// Adopter-linked `djogi` CLI integration tests (#370, plan Task 9).
//
// Each test builds a fixture `djogi` binary (or the workspace's own
// standalone `djogi`) and runs it as a subprocess, asserting the
// descriptor-provider boundary end to end:
//
//   * forced fixture (`adopter_app`) links BOTH model crates → all three
//     models discovered by `schema` AND `compose`;
//   * unforced fixture (`adopter_app_unforced`) references only `tracker`
//     → the `billing` crate dead-strips, so `Invoice` is invisible and the
//     linkage-aware drop guard fires;
//   * the standalone published `djogi` links zero models → descriptor
//     commands refuse with the §5.6 dual-cause diagnostic, while `verify`
//     degrades to snapshot-only when on-disk snapshots exist.
//
// Discovery is asserted against the real migration artifacts compose
// writes — committed up-SQL under `migrations/<db>/<app>/<version>.sdjql`
// and pending JSON under `target/djogi_pending/` — never a workspace-root
// `schema_snapshot.json` (compose writes no such file).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use djogi::testing::cli::{
    current_database, djogi_binary_path, temp_workspace, write_minimal_djogi_toml,
};

static CACHED_CARGO_DJOGI_BINARY: OnceLock<PathBuf> = OnceLock::new();
static CACHED_DJOGI_IN_WORKSPACE_BINARIES: OnceLock<Mutex<HashMap<String, PathBuf>>> =
    OnceLock::new();
static CACHED_ELEPHANT_TRACKER_BINARIES: OnceLock<Mutex<HashMap<String, PathBuf>>> =
    OnceLock::new();
static CACHED_FIXTURE_BINARIES: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();

fn cached_path(
    cache: &'static OnceLock<Mutex<HashMap<String, PathBuf>>>,
    key: String,
    build: impl FnOnce() -> PathBuf,
) -> PathBuf {
    let lock = cache.get_or_init(|| Mutex::new(HashMap::new()));

    {
        let guard = lock.lock().expect("acquire cached binary lock");
        if let Some(path) = guard.get(&key) {
            return path.clone();
        }
    }

    let built = build();

    let mut guard = lock.lock().expect("acquire cached binary lock");
    guard.entry(key).or_insert(built.clone()).clone()
}

/// Parse Cargo `--message-format=json` stdout to extract the artifact path
/// for the requested binary. Profile-agnostic: reads actual paths from Cargo
/// output rather than assuming `target/debug/<bin>` or `target/release/<bin>`
/// layout, so cross-compilation targets are handled correctly.
fn discover_binary_path_str(
    cargo_stdout: &str,
    bin_name: &str,
) -> Result<String, String> {
    for line in cargo_stdout.lines() {
        let msg: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if msg.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }
        let target_kind: Vec<&str> = msg
            .get("target")
            .and_then(|t| t.get("kind"))
            .and_then(|k| k.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        if !target_kind.contains(&"bin") {
            continue;
        }
        let bin = match msg
            .get("target")
            .and_then(|t| t.get("name"))
            .and_then(|n| n.as_str())
        {
            Some(n) => n,
            None => continue,
        };
        if bin != bin_name {
            continue;
        }
        if let Some(filenames) = msg.get("filenames").and_then(|f| f.as_array()) {
            for file in filenames.iter().filter_map(|f| f.as_str()) {
                if !file.ends_with(".d") && !file.ends_with(".pdb") {
                    return Ok(file.to_string());
                }
            }
        }
    }
    Err(format!("no binary artifact found for '{bin_name}'"))
}

// ── Path resolution ─────────────────────────────────────────────────────────

/// The `djogi-cli` crate root. `CARGO_MANIFEST_DIR` resolves to the
/// `djogi-cli/` crate directory when this integration test runs (cargo's
/// standard convention).
fn cli_crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The djogi workspace root — exactly ONE parent of the crate dir
/// (`djogi-cli` is a top-level workspace member, so the root is
/// `CARGO_MANIFEST_DIR/..`).
fn workspace_root() -> PathBuf {
    cli_crate_dir()
        .parent()
        .expect("djogi-cli has a parent (the workspace root)")
        .to_path_buf()
}

fn write_minimal_djogi_toml_with_cli(
    workspace: &Path,
    db_url: &str,
    cli_package: &str,
    cli_bin: &str,
) {
    let toml = format!(
        r#"profile = "development"

[database]
url = "{db_url}"

[server]
host = "127.0.0.1"
port = 0

[cli]
package = "{cli_package}"
bin = "{cli_bin}"
"#,
    );
    std::fs::write(workspace.join("Djogi.toml"), toml).expect("write Djogi.toml");
}

fn copy_elephant_tracker_workspace() -> PathBuf {
    let src = workspace_root().join("examples/elephant-tracker");
    let dst = temp_workspace("elephant-tracker-cargo-djogi");

    copy_dir_recursive(&src.join("src"), &dst.join("src"));
    copy_dir_recursive(&src.join("seeds"), &dst.join("seeds"));
    for file in ["Cargo.toml", "Djogi.toml", "README.md"] {
        let target = dst.join(file);
        std::fs::copy(src.join(file), target).expect("copy elephant-tracker file");
    }
    std::fs::copy(workspace_root().join("Cargo.lock"), dst.join("Cargo.lock"))
        .expect("copy workspace Cargo.lock");

    let manifest = dst.join("Cargo.toml");
    let manifest_text = std::fs::read_to_string(&manifest).expect("read copied Cargo.toml");
    let patched = manifest_text
        .replace(
            "path = \"../../djogi\"",
            &format!("path = \"{}\"", workspace_root().join("djogi").display()),
        )
        .replace(
            "path = \"../../djogi-cli\"",
            &format!("path = \"{}\"", workspace_root().join("djogi-cli").display()),
        );
    if patched != manifest_text {
        std::fs::write(&manifest, patched).expect("rewrite copied Cargo.toml");
    }

    dst
}

fn build_elephant_tracker_binary(
    manifest: &Path,
    target_subdir: &str,
    package: &str,
    bin: &str,
) -> PathBuf {
    let _ = target_subdir;
    let key = format!("elephant-tracker|{package}|{bin}");
    cached_path(&CACHED_ELEPHANT_TRACKER_BINARIES, key, || {
        let target_dir = workspace_root().join("target").join(format!("elephant-tracker-{package}-{bin}"));
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let output = Command::new(&cargo)
            .arg("build")
            // No `--locked`: this builds from a copied fixture whose Cargo.lock
            // may diverge from the workspace resolution (patched paths → different
            // dependency graph). The build is still isolated via `--target-dir`.
            .arg("--package")
            .arg(package)
            .arg("--bin")
            .arg(bin)
            .arg("--manifest-path")
            .arg(manifest)
            .arg("--target-dir")
            .arg(&target_dir)
            .arg("--message-format")
            .arg("json")
            .output()
            .expect("spawn cargo build");
        assert!(
            output.status.success(),
            "build failed for package '{package}', bin '{bin}': {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        PathBuf::from(
            discover_binary_path_str(&stdout, bin)
                .unwrap_or_else(|_| panic!("no binary artifact for '{bin}' in cargo output")),
        )
    })
}

fn copy_dir_recursive(src: &Path, dst: &Path) {
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        assert!(
            std::fs::create_dir_all(&dst_dir).is_ok(),
            "create dir {}",
            dst_dir.display()
        );
        let entries = match std::fs::read_dir(&src_dir) {
            Ok(entries) => entries,
            Err(err) => panic!("read_dir {}: {err}", src_dir.display()),
        };
        for entry in entries.flatten() {
            let src_path = entry.path();
            let dst_path = dst_dir.join(entry.file_name());
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(err) => panic!("file_type {}: {err}", src_path.display()),
            };
            if file_type.is_dir() {
                stack.push((src_path, dst_path));
            } else {
                if let Err(err) = std::fs::copy(&src_path, &dst_path) {
                    panic!("copy {} -> {}: {err}", src_path.display(), dst_path.display());
                }
            }
        }
    }
}

fn rebind_fixture_workspace_paths(workspace: &Path, repo_root: &Path) {
    let djogi_path = repo_root.join("djogi");
    let djogi_cli_path = repo_root.join("djogi-cli");
    let djogi_macros_path = repo_root.join("djogi-macros");
    let workspace_canon = workspace
        .canonicalize()
        .unwrap_or_else(|err| panic!("canonicalize workspace {}: {err}", workspace.display()));

    for rel in ["tracker/Cargo.toml", "billing/Cargo.toml", "bin/Cargo.toml"] {
        let manifest = workspace.join(rel);
        if !manifest.exists() {
            continue;
        }
        let manifest_canon = manifest
            .canonicalize()
            .unwrap_or_else(|err| panic!("canonicalize manifest {}: {err}", manifest.display()));
        if !manifest_canon.starts_with(&workspace_canon) || !manifest_canon.is_file() {
            continue;
        }

        let text = match std::fs::read_to_string(&manifest_canon) {
            Ok(content) => content,
            Err(err) => panic!("read {}: {err}", manifest_canon.display()),
        };
        let patched = text
            .replace("path = \"../../../../../djogi\"", &format!("path = \"{}\"", djogi_path.display()))
            .replace("path = \"../../../../../djogi-cli\"", &format!("path = \"{}\"", djogi_cli_path.display()))
            .replace("path = \"../../../../../djogi-macros\"", &format!("path = \"{}\"", djogi_macros_path.display()));
        if patched != text && let Err(err) = std::fs::write(&manifest_canon, patched) {
            panic!("write {}: {err}", manifest_canon.display());
        }
    }
}

fn copy_fixture_to_temp(fixture: &str) -> PathBuf {
    let src = cli_crate_dir().join("tests/fixtures").join(fixture);
    let dst = temp_workspace(&format!("adopter-fixture-{fixture}"));
    copy_dir_recursive(&src, &dst);
    rebind_fixture_workspace_paths(&dst, &workspace_root());
    dst
}

fn build_djogi_in_workspace(manifest: &Path, target_subdir: &str, package: &str, bin: &str) -> PathBuf {
    let _ = target_subdir;
    let key = format!("workspace:{manifest:?}|{package}|{bin}");
    cached_path(&CACHED_DJOGI_IN_WORKSPACE_BINARIES, key, || {
        let target_dir = workspace_root().join("target").join(format!("workspace-{package}-{bin}"));
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let output = Command::new(&cargo)
            .arg("build")
            .arg("--locked")
            .arg("--bin")
            .arg(bin)
            .arg("--package")
            .arg(package)
            .arg("--manifest-path")
            .arg(manifest)
            .arg("--target-dir")
            .arg(&target_dir)
            .arg("--message-format")
            .arg("json")
            .output()
            .expect("spawn cargo build");
        assert!(
            output.status.success(),
            "build for manifest {} failed: {}",
            manifest.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        PathBuf::from(
            discover_binary_path_str(&stdout, bin)
                .unwrap_or_else(|_| panic!("no binary artifact for '{bin}' in cargo output")),
        )
    })
}

fn build_cargo_djogi_binary() -> PathBuf {
    if let Some(path) = CACHED_CARGO_DJOGI_BINARY.get() {
        return path.clone();
    }

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let root = workspace_root();
    let target_dir = root.join("target").join("cargo-djogi-tests");
    let output = Command::new(&cargo)
        .arg("build")
        .arg("-p")
        .arg("cargo-djogi")
        .arg("--locked")
        .arg("--target-dir")
        .arg(&target_dir)
        .arg("--message-format")
        .arg("json")
        .output()
        .expect("spawn cargo build -p cargo-djogi");
    assert!(
        output.status.success(),
        "build cargo-djogi test binary failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = PathBuf::from(
        discover_binary_path_str(&stdout, "cargo-djogi")
            .unwrap_or_else(|_| panic!("no binary artifact for 'cargo-djogi' in cargo output")),
    );
    let _ = CACHED_CARGO_DJOGI_BINARY.set(path.clone());
    path
}

// ── Fixture / binary builders ────────────────────────────────────────────────

/// Build the named fixture's `djogi` binary and return its path.
///
/// The fixtures are three-crate workspaces under
/// `djogi-cli/tests/fixtures/<fixture>/`. The `djogi` binary lives in the
/// `bin` member, so the build targets the workspace manifest with
/// `--bin djogi`. Artifacts land under the OUTER workspace `target/` (a
/// dedicated subdir) via `--target-dir`, with `CARGO_TARGET_DIR` cleared so
/// the parent invocation's env does not leak in. `--locked` keeps the
/// fixture build deterministic against the committed `Cargo.lock`.
fn build_fixture_djogi(fixture: &str, target_subdir: &str) -> PathBuf {
    let _ = target_subdir;
    let key = fixture.to_string();
    cached_path(&CACHED_FIXTURE_BINARIES, key, || {
    let manifest = cli_crate_dir()
        .join("tests/fixtures")
        .join(fixture)
        .join("Cargo.toml");
    assert!(
        manifest.is_file(),
        "fixture manifest not found: {}",
        manifest.display()
    );
        let target_dir = workspace_root()
            .join("target")
            .join(format!("fixture-{fixture}"));
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(&cargo)
        .arg("build")
        // Match the CI release link profile the linkage spike used.
        .arg("--release")
        .arg("--locked")
        .arg("--bin")
        .arg("djogi")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target_dir)
        .env_remove("CARGO_TARGET_DIR")
        .arg("--message-format")
        .arg("json")
        .output()
        .expect("spawn cargo build for fixture");
    assert!(
        output.status.success(),
        "fixture {fixture} failed to build: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
        PathBuf::from(
        discover_binary_path_str(&stdout, "djogi")
            .unwrap_or_else(|_| panic!("no binary artifact for 'djogi' in cargo output")),
        )
    })
}

// ── Workspace / output helpers ───────────────────────────────────────────────

/// Make a fresh temp dir and write a minimal `Djogi.toml`. For
/// `schema` / `compose` (no DB) a dummy URL suffices — neither opens a
/// Postgres connection.
fn tempdir_with_djogi_toml() -> PathBuf {
    let dir = temp_workspace("adopter-fixture");
    write_minimal_djogi_toml(&dir, "postgres://localhost/none");
    dir
}

/// Run `<bin> schema --format json` in `work`, assert success, return
/// stdout as a `String`. `schema` reads the linked inventory and needs no
/// workspace flag — it runs in `work` purely so a `Djogi.toml` is present
/// for the (unused-by-schema) config surface.
fn run_schema_json(bin: &Path, work: &Path) -> String {
    let out = Command::new(bin)
        .args(["schema", "--format", "json"])
        .current_dir(work)
        .output()
        .expect("run fixture djogi schema");
    assert!(
        out.status.success(),
        "schema exit: {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Walk `work/migrations/` recursively and concatenate every committed
/// up-SQL file (`*.sdjql`, excluding `*.down.sdjql`). The fixture models
/// carry no `#[model(app = …)]`, so they project into the synthetic global
/// bucket and their `CREATE TABLE`s land under
/// `migrations/main/_global_/<version>.sdjql`.
fn read_all_composed_up_sql(work: &Path) -> String {
    let mut out = String::new();
    for path in walk_sdjql(&work.join("migrations")) {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.ends_with(".down.sdjql") {
            continue; // up-SQL only
        }
        if let Ok(contents) = std::fs::read_to_string(&path) {
            out.push_str(&contents);
            out.push('\n');
        }
    }
    out
}

fn read_normalized_composed_up_sql(work: &Path) -> Vec<String> {
    let mut up_sql = Vec::new();
    for path in walk_sdjql(&work.join("migrations")) {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.ends_with(".down.sdjql") {
            continue;
        }
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) => panic!("read {}: {err}", path.display()),
        };
        let mut normalized = contents.replace("\r\n", "\n");
        normalized = normalized
            .lines()
            .filter(|line| !line.trim_start().starts_with("-- Version:"))
            .collect::<Vec<_>>()
            .join("\n");
        normalized = normalized.replace("  ", " ");
        normalized = normalized.trim().to_string();
        if !normalized.is_empty() {
            up_sql.push(normalized);
        }
    }
    up_sql.sort();
    up_sql
}

/// Concatenate the contents of every `*.sdjql` (up AND down) under
/// `work/migrations/` — used to assert no DROP statement was emitted.
fn read_all_sdjql(work: &Path) -> String {
    let mut out = String::new();
    for path in walk_sdjql(&work.join("migrations")) {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            out.push_str(&contents);
            out.push('\n');
        }
    }
    out
}

/// Recursively collect every `*.sdjql` path under `root` (a small explicit
/// stack walk — `walkdir` is not a dependency). Returns an empty vector
/// when `root` does not exist.
fn walk_sdjql(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|e| e.to_str()) == Some("sdjql") {
                found.push(path);
            }
        }
    }
    found
}

/// True when `path` has no directory entries (or does not exist).
fn dir_is_empty(path: &Path) -> bool {
    std::fs::read_dir(path)
        .map(|mut d| d.next().is_none())
        .unwrap_or(true)
}

/// Resolve `DATABASE_URL` from the environment for the DB-backed tests.
fn database_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL not set")
}

/// Replace the database name in a PostgreSQL connection URL while
/// preserving the scheme, credentials, host, and port. Used to splice the
/// per-test database name (a bare NAME from `current_database`) into the
/// harness `DATABASE_URL` so the spawned `djogi` connects to the
/// provisioned per-test DB rather than treating the bare name as a URL.
fn splice_database_name(base_url: &str, db_name: &str) -> String {
    // Postgres URL format: postgres://[user[:password]@]host[:port]/database
    if let Some(slash_pos) = base_url.rfind('/') {
        let prefix = &base_url[..slash_pos + 1];
        return format!("{prefix}{db_name}");
    }
    // Fallback: if URL has no slash (malformed), just use base as-is.
    base_url.to_string()
}

/// Write a committed `billing`-app snapshot recording one table
/// (`invoices`) plus `registered_apps: ["billing"]` at the canonical
/// `migrations/main/billing/schema_snapshot.json` path — exactly where
/// `compose` reads committed snapshots from. Writing it to the workspace
/// ROOT (as an earlier driver did) would leave compose blind to it, so the
/// differ would never compute the DROP and the linkage guard would never
/// fire.
///
/// The snapshot is built from djogi's typed `AppliedSchema` and persisted
/// via [`djogi::migrate::save_snapshot`], so it stays byte-valid against
/// the live snapshot format (a hand-written JSON blob would silently rot
/// when the schema structs evolve).
fn write_billing_snapshot_with_table(work: &Path) {
    use djogi::migrate::{
        AppliedSchema, BucketKey, ColumnSchema, PkKindSchema, PrimaryKeySchema,
        SNAPSHOT_FORMAT_VERSION, TableSchema,
    };
    use std::collections::BTreeMap;

    let bucket = BucketKey {
        database: "main".to_string(),
        app: "billing".to_string(),
    };

    let id_column = ColumnSchema {
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
    };
    let reference_column = ColumnSchema {
        // A plain TEXT column — no server default, unlike the PK.
        default_sql: None,
        name: "reference".to_string(),
        sql_type: "TEXT".to_string(),
        ..id_column.clone()
    };

    let invoices = TableSchema {
        app: Some("billing".to_string()),
        columns: vec![id_column, reference_column],
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
        storage_params: None,
        table: "invoices".to_string(),
        table_comment: None,
        tablespace: None,
        tenant_key: None,
    };

    let mut models = BTreeMap::new();
    models.insert("invoices".to_string(), invoices);

    let snapshot = AppliedSchema {
        djogi_version: "0.1.0".to_string(),
        enums: BTreeMap::new(),
        format_version: SNAPSHOT_FORMAT_VERSION.to_string(),
        generated_at: "2026-04-25T00:00:00Z".to_string(),
        indexes: Vec::new(),
        models,
        registered_apps: vec!["billing".to_string()],
    };

    let path = djogi::migrate::snapshot_path(work, &bucket);
    djogi::migrate::save_snapshot(&snapshot, &path).expect("write billing schema_snapshot.json");
}

// ── POS: compose + schema discover all models from the provider ────────────

#[test]
fn adopter_binary_discovers_all_models() {
    let bin = build_fixture_djogi("adopter_app", "adopter_app_fixture");
    let work = tempdir_with_djogi_toml();

    // Discovery via schema (cheapest, no DB).
    let json = run_schema_json(&bin, &work);
    assert!(json.contains("elephants"), "Elephant missing: {json}");
    assert!(json.contains("herds"), "Herd missing: {json}");
    assert!(json.contains("invoices"), "Invoice missing: {json}");

    // Also prove COMPOSE discovers all three (the actual migration path,
    // not just introspection). compose writes pending JSON + committed SQL
    // with no DB connection.
    let compose = Command::new(&bin)
        .args(["migrations", "compose", "--name", "init"])
        .current_dir(&work)
        .output()
        .expect("run fixture djogi compose");
    assert!(
        compose.status.success(),
        "compose exit: {:?}\nstderr: {}",
        compose.status,
        String::from_utf8_lossy(&compose.stderr)
    );
    assert!(
        work.join("target/djogi_pending").exists(),
        "compose must write pending artifacts under target/djogi_pending"
    );
    let composed_sql = read_all_composed_up_sql(&work);
    for table in ["elephants", "herds", "invoices"] {
        assert!(
            composed_sql.contains(table),
            "compose missed {table}:\n{composed_sql}"
        );
    }
}

// ── NAME: command-name contract ────────────────────────────────────────────

#[test]
fn binary_is_named_djogi_and_surface_matches() {
    let bin = build_fixture_djogi("adopter_app", "adopter_app_fixture");
    assert_eq!(
        bin.file_name().expect("fixture bin has a file name"),
        "djogi",
        "fixture bin must be named `djogi`, not a project-specific name"
    );
    // The surface is `djogi migrations …`, not `cargo djogi` and not a
    // project-specific command.
    let out = Command::new(&bin)
        .args(["migrations", "--help"])
        .output()
        .expect("run djogi migrations --help");
    assert!(
        out.status.success(),
        "`djogi migrations --help` must succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("compose"),
        "migrations surface must expose `compose`: {help}"
    );
}

// ── LINK: force vs no-force visibility (cross-crate dead-strip) ─────────────

#[test]
fn link_unforced_crate_is_invisible_forced_is_visible() {
    let forced = build_fixture_djogi("adopter_app", "adopter_app_fixture");
    let unforced = build_fixture_djogi("adopter_app_unforced", "adopter_app_unforced_fixture");
    let work = tempdir_with_djogi_toml();

    let forced_json = run_schema_json(&forced, &work);
    assert!(
        forced_json.contains("invoices"),
        "billing forced ⇒ Invoice visible: {forced_json}"
    );
    assert!(
        forced_json.contains("elephants"),
        "tracker forced ⇒ Elephant visible: {forced_json}"
    );

    let unforced_json = run_schema_json(&unforced, &work);
    assert!(
        unforced_json.contains("elephants"),
        "tracker is forced in both fixtures ⇒ Elephant visible: {unforced_json}"
    );
    // Load-bearing assertion: `billing` is a SEPARATE crate the unforced
    // bin references nothing from, so the linker drops it whole — the
    // partial-miss hazard. Robust regardless of the intra-crate
    // sibling-strip question (which the spike settled).
    assert!(
        !unforced_json.contains("invoices"),
        "billing UNFORCED ⇒ Invoice invisible (the partial-miss hazard): {unforced_json}"
    );
}

// ── DROPGUARD: linkage guard fires with --allow-destructive, zero DROPs ─────

#[test]
fn dropguard_unlinked_app_refuses_even_with_allow_destructive() {
    let unforced = build_fixture_djogi("adopter_app_unforced", "adopter_app_unforced_fixture");
    let work = tempdir_with_djogi_toml();
    // Seed a committed snapshot for the `billing` app with one table +
    // registered_apps. The unforced binary projects ZERO models for
    // `billing`, so the linkage-aware drop guard must refuse.
    write_billing_snapshot_with_table(&work);

    let out = Command::new(&unforced)
        .args([
            "migrations",
            "compose",
            "--allow-destructive",
            "--name",
            "drop_billing",
        ])
        .current_dir(&work)
        .output()
        .expect("run unforced compose");

    assert_eq!(
        out.status.code(),
        Some(2),
        "linkage guard must refuse (exit 2) even with --allow-destructive; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no models for it are linked now"),
        "linkage-specific hint expected: {stderr}"
    );
    assert!(
        stderr.contains("billing"),
        "diagnostic must name the unlinked app: {stderr}"
    );

    // FIX-2: assert ZERO DROP statements were emitted. The guard fires at
    // compose's step 2b — AFTER Phase-0 auto-emit (step 0) but BEFORE the
    // differ (step 3). So:
    //
    //   * the billing app gets NO migration file at all — its directory
    //     holds only the seed `schema_snapshot.json`. The DROP-TABLE
    //     migration the guard prevented is therefore absent;
    //   * no `*.sdjql` anywhere contains a DROP. The only migration written
    //     is the Phase-0 bootstrap (comment-only down, no DROP). Because the
    //     differ never ran, the linked tracker models produced no migration
    //     either — a successful run WOULD emit a tracker CREATE whose `.down`
    //     contains `DROP TABLE`, so a guard that leaked past step 3 would
    //     trip this assertion. That makes the check non-tautological;
    //   * no billing pending JSON was staged. (Phase-0's own
    //     `target/djogi_pending/main/.phase_zero/`
    //     `V00000000000000__phase_zero_bootstrap.json` legitimately exists —
    //     Phase-0 emission precedes the guard — so the bare directory's
    //     existence is NOT a refusal violation; the billing-specific pending
    //     artifact is.)
    let billing_dir = work.join("migrations/main/billing");
    let billing_migrations: Vec<_> = walk_sdjql(&billing_dir);
    assert!(
        billing_migrations.is_empty(),
        "guard must emit no billing migration; found: {billing_migrations:?}"
    );
    let all_sql = read_all_sdjql(&work);
    assert!(
        !all_sql.to_uppercase().contains("DROP"),
        "guard must emit no DROP statement (differ never ran); found SQL:\n{all_sql}"
    );
    assert!(
        !work.join("target/djogi_pending/main/billing.json").exists(),
        "guard refusal must stage no billing pending artifact"
    );
}

// ── PARITY: schema and compose see the same models (compose output) ────────

#[test]
fn parity_schema_and_compose_see_same_models() {
    // Parity is proven on COMPOSE output, not just schema output — schema
    // and compose could diverge if only one reads the provider. Run BOTH
    // and assert the same three tables appear in schema JSON AND in the
    // composed migration artifacts.
    let bin = build_fixture_djogi("adopter_app", "adopter_app_fixture");
    let work = tempdir_with_djogi_toml();

    // 1. schema sees all three.
    let schema_json = run_schema_json(&bin, &work);
    for table in ["elephants", "herds", "invoices"] {
        assert!(schema_json.contains(table), "schema must see {table}");
    }

    // 2. compose (no DB needed) sees all three. Assert against the actual
    //    composed artifacts, so a compose path that ignored the provider
    //    would fail here even if schema passed.
    let compose = Command::new(&bin)
        .args(["migrations", "compose", "--name", "init"])
        .current_dir(&work)
        .output()
        .expect("run compose");
    assert!(
        compose.status.success(),
        "compose must succeed: {}",
        String::from_utf8_lossy(&compose.stderr)
    );
    let composed_sql = read_all_composed_up_sql(&work);
    for table in ["elephants", "herds", "invoices"] {
        assert!(
            composed_sql.contains(table),
            "compose parity: {table} must appear in the composed SQL (== schema). \
             A compose path that bypassed the provider would miss it.\nSQL:\n{composed_sql}"
        );
    }
}

#[test]
fn parity_cargo_djogi_matches_direct_adopter_bin_compose() {
    let wrapper_bin = build_cargo_djogi_binary();

    // Keep builds/artifacts isolated for direct and wrapper runs.
    let direct_workspace = copy_fixture_to_temp("adopter_app");
    let wrapper_workspace = copy_fixture_to_temp("adopter_app");
    let cli_package = "adopter-app-bin";
    let cli_bin = "djogi";

    write_minimal_djogi_toml_with_cli(&direct_workspace, "postgres://localhost/none", cli_package, cli_bin);
    write_minimal_djogi_toml_with_cli(&wrapper_workspace, "postgres://localhost/none", cli_package, cli_bin);

    let direct_manifest = direct_workspace.join("Cargo.toml");
    let direct_bin = build_djogi_in_workspace(&direct_manifest, "direct_adopter_app", cli_package, cli_bin);
    let direct_out = Command::new(&direct_bin)
        .args(["migrations", "compose", "--name", "parity"])
        .current_dir(&direct_workspace)
        .output()
        .expect("run direct adopter compose");
    assert!(
        direct_out.status.success(),
        "direct adopter compose must succeed: {}",
        String::from_utf8_lossy(&direct_out.stderr)
    );

    let wrapper_out = Command::new(&wrapper_bin)
        .args(["migrations", "compose", "--name", "parity"])
        .current_dir(&wrapper_workspace)
        .output()
        .expect("run cargo-djogi compose");
    assert!(
        wrapper_out.status.success(),
        "cargo djogi compose must succeed: {}",
        String::from_utf8_lossy(&wrapper_out.stderr)
    );

    let direct_sql = read_normalized_composed_up_sql(&direct_workspace);
    let wrapper_sql = read_normalized_composed_up_sql(&wrapper_workspace);
    assert!(
        !direct_sql.is_empty(),
        "direct compose must create up-SQL artifacts"
    );
    assert!(
        !wrapper_sql.is_empty(),
        "wrapper compose must create up-SQL artifacts"
    );
    assert_eq!(direct_sql, wrapper_sql, "compose output through cargo djogi should match direct adopter djogi");
}

#[test]
fn elephant_tracker_binary_does_not_expose_migrate_or_seed_commands() {
    let workspace = copy_elephant_tracker_workspace();
    let manifest = workspace.join("Cargo.toml");
    let bin = build_elephant_tracker_binary(
        &manifest,
        "elephant-tracker-no-migrate-seed",
        "elephant-tracker",
        "elephant-tracker",
    );

    let out = Command::new(&bin)
        .arg("--help")
        .output()
        .expect("run elephant-tracker --help");
    assert!(
        out.status.success(),
        "elephant-tracker --help exit: {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        !help.contains("migrate"),
        "elephant-tracker should not expose a `migrate` subcommand once cargo djogi is the adopter CLI path"
    );
    assert!(
        !help.contains("seed"),
        "elephant-tracker should not expose a `seed` subcommand once cargo djogi is the adopter CLI path"
    );
    assert!(
        help.contains("demo"),
        "elephant-tracker should still expose `demo` commands"
    );
}

#[test]
fn elephant_tracker_cargo_djogi_compose_uses_adopter_descriptors() {
    let workspace = copy_elephant_tracker_workspace();
    let manifest = workspace.join("Cargo.toml");
    let wrapper = build_elephant_tracker_binary(
        &manifest,
        "elephant-tracker-cargo-djogi",
        "elephant-tracker",
        "elephant-tracker-djogi",
    );

    let out = Command::new(&wrapper)
        .args(["migrations", "compose", "--name", "init"])
        .current_dir(&workspace)
        .env("DATABASE_URL", "postgres://localhost/none")
        .output()
        .expect("run cargo-djogi compose in copied elephant-tracker workspace");
    assert!(
        out.status.success(),
        "cargo djogi compose must succeed in elephant-tracker workspace: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let composed_sql = read_all_composed_up_sql(&workspace);
    assert!(!composed_sql.is_empty(), "compose output should produce up-SQL");
    for table in ["countries", "herds", "elephants", "sightings", "researchers"] {
        assert!(
            composed_sql.contains(table),
            "elephant-tracker compose should include table `{table}`"
        );
    }
}

// ── VERIFY-DEGRADE: zero-descriptor verify degrades to snapshot-only ───────

/// The model whose live table the degrade test seeds via the typed
/// surface. Identical shape to the `billing` fixture crate's `Invoice`
/// (`tests/fixtures/adopter_app/billing/src/lib.rs`): the synthetic global
/// bucket (no `#[model(app = …)]`), table `invoices`, one `reference`
/// field. Declaring it here lets the test process `sync_models` it into the
/// per-test DB AND project it into the on-disk `billing` snapshot, so the
/// snapshot-vs-live comparison is a TRUE structural match by construction —
/// not a hand-built 2-column snapshot that would diverge from a real
/// model's framework-injected columns.
///
/// Wrapped in its own module so the `use djogi::prelude::*` that brings the
/// `#[derive(Model)]` macro + its `#[model]` helper attribute into scope
/// does not leak into the rest of this large test file.
mod degrade_model {
    use djogi::prelude::*;

    #[derive(Model)]
    #[model(table = "invoices")]
    pub struct DegradeInvoice {
        pub reference: String,
    }
}
use degrade_model::DegradeInvoice;

/// Write a committed `billing`-bucket snapshot whose `invoices` table is
/// PROJECTED FROM [`DegradeInvoice`] — the same model the test syncs into
/// the live DB — so the snapshot carries the identical column set
/// (`id`/`created_at`/`updated_at`/`reference`) Postgres will actually have.
/// A correct snapshot therefore validates cleanly against the seeded live
/// table with no spurious missing-table / missing-column drift.
///
/// `DegradeInvoice` projects into the synthetic global bucket `(main, "")`;
/// since Postgres has no per-app schema (every app's tables live in
/// `public`), the table is re-keyed under the `billing` bucket here and the
/// table's `app` tag set to `billing`. The verify differ compares table /
/// column / PK / index shape — never the bucket label — so this re-tag is
/// transparent to the comparison while exercising the NAMED-bucket
/// standalone path (the global bucket is unaffected by the #370 bug).
fn write_billing_snapshot_projected_from_model(work: &Path) {
    use djogi::migrate::{
        AppliedSchema, BucketKey, DescriptorProvider, project_from_provider, save_snapshot,
        snapshot_path,
    };

    // Focused provider: only DegradeInvoice for models(); inventory for the
    // rest so apps()/enums()/deferrability resolve normally.
    struct OnlyDegradeInvoice;
    impl DescriptorProvider for OnlyDegradeInvoice {
        fn models(&self) -> Vec<&'static djogi::descriptor::ModelDescriptor> {
            vec![<DegradeInvoice as djogi::model::Model>::descriptor()]
        }
        fn enums(&self) -> Vec<&'static djogi::descriptor::EnumDescriptor> {
            djogi::migrate::InventoryDescriptorProvider::new().enums()
        }
        fn apps(&self) -> &'static [djogi::AppDescriptor] {
            djogi::migrate::InventoryDescriptorProvider::new().apps()
        }
        fn deferrability_specs(&self) -> Vec<&'static djogi::descriptor::DeferrabilitySpec> {
            djogi::migrate::InventoryDescriptorProvider::new().deferrability_specs()
        }
    }

    let projected = project_from_provider(&OnlyDegradeInvoice).expect("project DegradeInvoice");
    let global = BucketKey {
        database: "main".to_string(),
        app: String::new(),
    };
    let global_schema = projected
        .get(&global)
        .expect("DegradeInvoice projects into the global bucket");
    let mut invoices = global_schema
        .models
        .get("invoices")
        .expect("global bucket carries the invoices table")
        .clone();
    // Re-tag the table to the billing app so the snapshot's bucket and the
    // table's `app` agree; the differ ignores `app`, so this only documents
    // intent.
    invoices.app = Some("billing".to_string());

    let mut models = std::collections::BTreeMap::new();
    models.insert("invoices".to_string(), invoices);

    let billing_bucket = BucketKey {
        database: "main".to_string(),
        app: "billing".to_string(),
    };
    let snapshot = AppliedSchema {
        registered_apps: vec!["billing".to_string()],
        models,
        ..global_schema.clone()
    };
    let path = snapshot_path(work, &billing_bucket);
    save_snapshot(&snapshot, &path).expect("write billing schema_snapshot.json");
}

#[djogi::djogi_test(sync_models = [DegradeInvoice])]
async fn verify_degrade_snapshot_only_against_valid_db(mut ctx: djogi::DjogiContext) {
    // The standalone published `djogi` links zero models. With an on-disk
    // NAMED-bucket snapshot present and a reachable DB that ACTUALLY HAS the
    // snapshot's table, verify must:
    //   (a) NOT emit the §5.6 dual-cause refusal (snapshots exist ⇒ degrade);
    //   (b) enumerate the `billing` bucket FROM the snapshot;
    //   (c) report the bucket VERIFIED CLEANLY — NO spurious missing-table
    //       (D601) / missing-column (D603) drift. Before the #370 fix,
    //       `live_schema_for_repair` scoped the NAMED bucket from the (empty)
    //       standalone inventory, filtering the live schema to nothing and
    //       reporting `invoices` as missing — FALSE drift. This assertion is
    //       the regression guard: a weak "contains drift OR verified" check
    //       would pass on that false drift, so we assert NO error/drift.
    //
    // `sync_models = [DegradeInvoice]` seeds the live `invoices` table via
    // the typed surface; the on-disk snapshot is projected from the SAME
    // model, so snapshot == live structurally.
    let _ = &mut ctx; // sync_models already ran via the macro; ctx kept for the URL splice below.

    let bin = djogi_binary_path();
    let work = temp_workspace("verify-degrade");

    // Per-test DB URL: splice the provisioned DB NAME into the harness
    // DATABASE_URL (current_database returns a bare name, not a URL). This
    // is the SAME per-test DB the macro seeded `invoices` into.
    let db_url = splice_database_name(&database_url(), &current_database(&mut ctx).await);
    write_minimal_djogi_toml(&work, &db_url);
    write_billing_snapshot_projected_from_model(&work);

    let out = Command::new(&bin)
        .args(["migrations", "verify"])
        .current_dir(&work)
        .env("DATABASE_URL", &db_url)
        .output()
        .expect("run verify");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // (a) NOT the zero-descriptor refusal.
    assert_ne!(
        out.status.code(),
        Some(2),
        "verify must degrade, not refuse (exit 2): stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stderr.contains("no djogi models are registered"),
        "verify must NOT emit the zero-descriptor diagnostic when snapshots exist: {stderr}"
    );

    // (b) verify enumerated the billing bucket from disk: its per-bucket
    //     header is rendered.
    assert!(
        stdout.contains("billing"),
        "verify must enumerate the on-disk billing bucket: stdout={stdout} stderr={stderr}"
    );

    // (c) THE REGRESSION GUARD: the seeded live DB has the snapshot's table,
    //     so a correct standalone verify reports NO drift. Assert clean exit
    //     and the absence of the table/column-missing diagnostics that the
    //     pre-fix false-drift bug produced.
    assert_eq!(
        out.status.code(),
        Some(0),
        "snapshot-only verify against a DB that HAS the table must exit 0 (clean), \
         not report false drift: stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stdout.contains("D601") && !stdout.contains("D603"),
        "no spurious missing-table (D601) / missing-column (D603) drift expected when the \
         live DB actually has the snapshot's table: stdout={stdout}"
    );
    assert!(
        !stdout.contains("missing from the live database"),
        "the snapshot's `invoices` table exists in the live DB — verify must not report it \
         missing (the #370 false-drift bug): stdout={stdout}"
    );
}

// ── T-NEG: standalone compose refuses (zero descriptors, before the differ) ──

#[test]
fn t_neg_standalone_compose_refuses_with_exit_2_and_no_artifacts() {
    // The standalone published `djogi` links zero models ⇒ compose must
    // emit the dual-cause diagnostic, exit 2 (before the differ), and write
    // NO pending artifacts / NO committed migrations.
    let bin = djogi_binary_path();
    let work = tempdir_with_djogi_toml(); // no snapshots — neither descriptors nor prior state
    let out = Command::new(&bin)
        .args(["migrations", "compose", "--name", "init"])
        .current_dir(&work)
        .output()
        .expect("run standalone compose");

    assert_eq!(
        out.status.code(),
        Some(2),
        "zero-descriptor compose must exit 2: stderr {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Dual-cause diagnostic CONTENT (§5.6): names the registration problem,
    // both standalone-vs-linked scenarios, and the `apply` fallback.
    assert!(
        stderr.contains("no djogi models are registered"),
        "dual-cause diagnostic header expected: {stderr}"
    );
    assert!(
        stderr.contains("standalone") && stderr.contains("apply"),
        "diagnostic must explain the standalone-vs-linked scenarios and the apply fallback: {stderr}"
    );

    // Nothing emitted (refused before the differ).
    assert!(
        !work.join("target/djogi_pending").exists(),
        "refusal must write no pending artifacts"
    );
    let migrations_dir = work.join("migrations");
    assert!(
        !migrations_dir.exists() || dir_is_empty(&migrations_dir),
        "refusal must write no committed migrations"
    );
}

// ── T-NOLOGIC: fixture sources contain only model defs + glue ────────────────

#[test]
fn t_nologic_fixture_has_no_custom_migration_code() {
    // The fixture crates contain only model defs + the glue — no compose /
    // apply reimplementation, no hand-maintained descriptor list. Reads the
    // SEPARATE crate sources (the separate-crate restructure).
    let fixtures = cli_crate_dir().join("tests/fixtures/adopter_app");
    let tracker_src =
        std::fs::read_to_string(fixtures.join("tracker/src/lib.rs")).expect("read tracker src");
    let billing_src =
        std::fs::read_to_string(fixtures.join("billing/src/lib.rs")).expect("read billing src");
    let bin_src =
        std::fs::read_to_string(fixtures.join("bin/src/bin/djogi.rs")).expect("read bin src");

    for (name, src) in [
        ("tracker", &tracker_src),
        ("billing", &billing_src),
        ("bin", &bin_src),
    ] {
        assert!(
            !src.contains("project_from"),
            "{name}: no projection reimpl"
        );
        assert!(!src.contains("compose("), "{name}: no compose reimpl");
        assert!(
            !src.contains("__bypass") && !src.contains("raw_"),
            "{name}: no raw_* / bypass (GH #133)"
        );
    }
    // The bin is only the glue: djogi_main! (or link refs) + run_from_env.
    assert!(
        bin_src.contains("djogi_main!") || bin_src.contains("run_from_env"),
        "bin must be glue only: {bin_src}"
    );
}

// ── T-NOCARGO: compose works with no Cargo on PATH and no source ─────────────

#[test]
fn nocargo_compose_without_cargo_or_source() {
    // Build the adopter fixture binary (links model crates via inventory).
    let bin = build_fixture_djogi("adopter_app", "adopter_app_fixture");

    // Copy binary + config to an isolated temp dir (no source code there).
    let runtime_dir = temp_workspace("nocargo");
    let copied_bin = runtime_dir.join("djogi");
    std::fs::copy(&bin, &copied_bin).expect("copy djogi binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&copied_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&copied_bin, perms).unwrap();
    }

    // Compose needs no DB — a dummy URL is fine.
    write_minimal_djogi_toml(&runtime_dir, "postgres://localhost/none");

    // Run compose with a PATH that contains NO cargo/toolchain.
    let empty_path = temp_workspace("nocargo-path");
    let out = Command::new(&copied_bin)
        .args(["migrations", "compose", "--name", "init"])
        .current_dir(&runtime_dir)
        .env("PATH", &empty_path) // no cargo/toolchain reachable
        .output()
        .expect("run copied djogi compose");

    assert!(
        out.status.success(),
        "compose must work with no cargo on PATH: stderr {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Pending artifacts exist — proof compose produced output without cargo.
    assert!(
        runtime_dir.join("target").join("djogi_pending").exists(),
        "compose wrote pending artifacts"
    );
}

// ── T-CONTAINER-APPLY: apply from prebuilt binary (no source, no Cargo) ──────

#[djogi::djogi_test]
async fn container_apply_from_prebuilt_binary(mut ctx: djogi::DjogiContext) {
    // Derive the per-test DB URL by splicing the test database name into the
    // harness DATABASE_URL.
    let db_url = splice_database_name(&database_url(), &current_database(&mut ctx).await);

    let bin = build_fixture_djogi("adopter_app", "adopter_app_fixture");
    let runtime_dir = temp_workspace("container-apply");
    let copied = runtime_dir.join("djogi");
    std::fs::copy(&bin, &copied).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&copied).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&copied, perms).unwrap();
    }

    write_minimal_djogi_toml(&runtime_dir, &db_url);
    let empty_path = temp_workspace("container-path");

    // Compose writes pending artifacts (including the Phase-0 bootstrap that
    // installs heerid_next()).
    let compose_out = Command::new(&copied)
        .args(["migrations", "compose", "--name", "init"])
        .current_dir(&runtime_dir)
        .env("PATH", &empty_path)
        .output()
        .expect("compose");
    assert!(
        compose_out.status.success(),
        "compose must succeed: {}",
        String::from_utf8_lossy(&compose_out.stderr)
    );

    // Apply with no cargo, no source — just binary + config + artifacts + DB.
    // Apply is descriptor-free here, but still identity-bearing; on a fresh
    // test database we use single-node-dev provisioning.
    let apply_out = Command::new(&copied)
        .args(["migrations", "apply", "--single-node-dev"])
        .current_dir(&runtime_dir)
        .env("PATH", &empty_path)
        .env("DATABASE_URL", &db_url)
        .output()
        .expect("apply");
    assert!(
        apply_out.status.success(),
        "apply must succeed: {}",
        String::from_utf8_lossy(&apply_out.stderr)
    );

    // Assert the migration landed via `migrations status` (typed CLI surface,
    // not raw SQL and not re-declaring models in this test crate).
    let status_out = Command::new(&copied)
        .args(["migrations", "status"])
        .current_dir(&runtime_dir)
        .env("PATH", &empty_path)
        .env("DATABASE_URL", &db_url)
        .output()
        .expect("status");
    assert!(
        status_out.status.success(),
        "status must succeed: {}",
        String::from_utf8_lossy(&status_out.stderr)
    );
    let status_text = String::from_utf8_lossy(&status_out.stdout);
    assert!(
        status_text.contains("applied") || !status_text.contains("pending"),
        "ledger must show the composed migration applied: {status_text}"
    );
}

// ── T-STANDALONE-APPLY: standalone binary applies pending artifacts ──────────

#[djogi::djogi_test]
async fn apply_with_pending_artifacts(mut ctx: djogi::DjogiContext) {
    // Use the adopter binary to compose (produces pending artifacts with live
    // descriptors), then use the standalone published djogi (zero descriptors)
    // to apply those artifacts — proving apply needs no live descriptors even
    // though it still requires node identity. This fresh test database uses
    // single-node-dev provisioning.
    let adopter = build_fixture_djogi("adopter_app", "adopter_app_fixture");
    let standalone = djogi_binary_path();

    let runtime_dir = temp_workspace("standalone-apply");
    let db_url = splice_database_name(&database_url(), &current_database(&mut ctx).await);
    write_minimal_djogi_toml(&runtime_dir, &db_url);

    // Adopter composes (has live descriptors from linked model crates).
    let compose_out = Command::new(&adopter)
        .args(["migrations", "compose", "--name", "init"])
        .current_dir(&runtime_dir)
        .env("DATABASE_URL", &db_url)
        .output()
        .expect("adopter compose");
    assert!(
        compose_out.status.success(),
        "adopter compose must succeed: {}",
        String::from_utf8_lossy(&compose_out.stderr)
    );

    // Standalone applies with single-node-dev identity and no descriptors.
    let apply_out = Command::new(&standalone)
        .args(["migrations", "apply", "--single-node-dev"])
        .current_dir(&runtime_dir)
        .env("DATABASE_URL", &db_url)
        .output()
        .expect("standalone apply");
    assert!(
        apply_out.status.success(),
        "standalone apply must succeed: {}",
        String::from_utf8_lossy(&apply_out.stderr)
    );

    // Assert the migration actually landed: run `migrations status` via the
    // standalone binary (descriptor-free commands work) and confirm the
    // composed migration is shown as applied, not pending.
    let status_out = Command::new(&standalone)
        .args(["migrations", "status"])
        .current_dir(&runtime_dir)
        .env("DATABASE_URL", &db_url)
        .output()
        .expect("standalone status");
    assert!(
        status_out.status.success(),
        "status must succeed after standalone apply: {}",
        String::from_utf8_lossy(&status_out.stderr)
    );
    let status_text = String::from_utf8_lossy(&status_out.stdout);
    assert!(
        status_text.contains("applied") || !status_text.contains("pending"),
        "ledger must show the composed migration applied after standalone apply: {status_text}"
    );
}

// ── T-FORBID-UNSAFE: adopter glue compiles under forbid(unsafe_code) ─────────

#[test]
fn t_forbid_unsafe_build_succeeds() {
    let bin = build_fixture_djogi("adopter_forbid_unsafe", "adopter_forbid_unsafe_fixture");
    assert!(
        bin.exists(),
        "forbid_unsafe binary should exist after a successful build"
    );
}

// ── Unit tests for discover_binary_path_str ──────────────────────────────────

#[test]
fn discover_binary_path_native_json() {
    let stdout = r#"{"reason":"compiler-artifact","target":{"name":"djogi","kind":["bin"]},"profile":"release","filenames":["/some/path/target/release/djogi"]}"#;
    let path = discover_binary_path_str(stdout, "djogi").unwrap();
    assert_eq!(path, "/some/path/target/release/djogi");
}

#[test]
fn discover_binary_path_cross_compiled_json() {
    let stdout = r#"{"reason":"compiler-artifact","target":{"name":"my_app","kind":["bin"]},"target_triple":"aarch64-unknown-linux-gnu","profile":"debug","filenames":["/some/path/target/aarch64-unknown-linux-gnu/debug/my_app"]}"#;
    let path = discover_binary_path_str(stdout, "my_app").unwrap();
    assert_eq!(path, "/some/path/target/aarch64-unknown-linux-gnu/debug/my_app");
}

#[test]
fn discover_binary_path_no_match() {
    let stdout = r#"{"reason":"compiler-artifact","target":{"name":"other_lib","kind":["rlib"]},"filenames":["/some/path/target/debug/libother.rlib"]}"#;
    let result = discover_binary_path_str(stdout, "djogi");
    assert!(result.is_err());
}

#[test]
fn discover_binary_path_multiline() {
    let stdout = r#"{"reason":"status","message":"compiling"}
{"reason":"compiler-artifact","target":{"name":"djogi","kind":["bin"]},"filenames":["/path/target/x86_64-unknown-linux-gnu/release/djogi"]}
{"reason":"build-finished","success":true}"#;
    let path = discover_binary_path_str(stdout, "djogi").unwrap();
    assert_eq!(path, "/path/target/x86_64-unknown-linux-gnu/release/djogi");
}
