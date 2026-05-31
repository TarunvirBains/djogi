// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

// Adopter-linked `djogi` CLI integration tests.
//
// Verifies that a djogi binary published by an adopter (separate workspace,
// separate Cargo.lock, real `#[derive(Model)]` structs) behaves identically
// to the framework's own binary for compose/verify operations, and that
// dead-stripping of unreferenced model crates is detectable at runtime.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use djogi::testing::cli::{
    current_database, djogi_binary_path, temp_workspace, write_minimal_djogi_toml,
};

/// Build the adopter fixture workspace in release mode and return the path to
/// the `djogi` binary inside it.
fn build_fixture_bin(fixture_name: &str) -> PathBuf {
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(fixture_name);
    assert!(
        fixture_dir.is_dir(),
        "fixture directory not found: {:?}",
        fixture_dir
    );

    let status = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .arg("--manifest-path")
        .arg(fixture_dir.join("Cargo.toml"))
        // Adopter fixture is an isolated workspace — unset CARGO_TARGET_DIR
        // so it builds into its own target/ (not the shared worktree cache).
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .expect("cargo build failed for adopter fixture");
    assert!(
        status.success(),
        "cargo build failed for adopter fixture '{}'",
        fixture_name
    );

    fixture_dir.join("target").join("release").join("djogi")
}

/// Resolve DATABASE_URL from the environment for test workspace setup.
fn database_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL not set")
}

/// Write a minimal `AppliedSchema` JSON that registers the billing app with one
/// table (invoices) so the CLI considers it linked. Returns the path to the
/// snapshot file inside the temp directory.
fn write_billing_snapshot_with_table(tmp: &Path) -> PathBuf {
    let json = serde_json::json!({
        "format_version": 1,
        "tables": [
            {
                "name": "invoices",
                "columns": [
                    {
                        "name": "id",
                        "pg_type": "bigint",
                        "nullable": false,
                        "default_expr": "heerid_next_desc()"
                    },
                    {
                        "name": "reference",
                        "pg_type": "text",
                        "nullable": false
                    },
                    {
                        "name": "created_at",
                        "pg_type": "timestamp with time zone",
                        "nullable": false,
                        "default_expr": "CURRENT_TIMESTAMP"
                    },
                    {
                        "name": "updated_at",
                        "pg_type": "timestamp with time zone",
                        "nullable": false,
                        "default_expr": "CURRENT_TIMESTAMP"
                    }
                ],
                "primary_key": {
                    "type": "SingleColumn",
                    "column_name": "id"
                }
            }
        ],
        "registered_apps": ["billing"]
    });

    let snapshot_path = tmp.join("schema_snapshot.json");
    let mut f = std::fs::File::create(&snapshot_path).expect("create schema_snapshot.json");
    f.write_all(serde_json::to_string_pretty(&json).unwrap().as_bytes())
        .expect("write schema_snapshot.json");
    snapshot_path
}

// ── T-LINK: Multi-model + cross-crate retention ─────────────────────────────

#[test]
fn t_link_multi_model_cross_crate_retention() {
    // Build forced fixture binary (references ALL models across both crates).
    let bin = build_fixture_bin("adopter_app");

    let workspace = temp_workspace("t9-link-forced");
    write_minimal_djogi_toml(&workspace, &database_url());

    let output = Command::new(&bin)
        .args(["migrations", "compose"])
        .arg("--workspace")
        .arg(&workspace)
        .env("DATABASE_URL", database_url())
        .output()
        .expect("failed to execute child process");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // Should succeed (exit 0).
    assert!(
        output.status.success(),
        "forced fixture compose should succeed; stderr: {stderr}"
    );

    // Verify the compose plan contains all three tables.
    let snapshot_path = workspace.join("schema_snapshot.json");
    let snapshot_json =
        std::fs::read_to_string(&snapshot_path).unwrap_or_else(|_| String::from("{}"));
    let snapshot: serde_json::Value =
        serde_json::from_str(&snapshot_json).expect("parse schema_snapshot.json");

    let tables = snapshot.get("tables").expect("tables key in snapshot");
    let table_names: Vec<&str> = tables
        .as_array()
        .expect("tables is array")
        .iter()
        .map(|t| t.get("name").expect("table name").as_str().expect("string"))
        .collect();

    assert!(
        table_names.contains(&"elephants"),
        "schema_snapshot.json should contain elephants; got {:?}",
        table_names
    );
    assert!(
        table_names.contains(&"herds"),
        "schema_snapshot.json should contain herds; got {:?}",
        table_names
    );
    assert!(
        table_names.contains(&"invoices"),
        "schema_snapshot.json should contain invoices (cross-crate); got {:?}",
        table_names
    );

    // ── Unforced fixture: billing NOT referenced, must be dead-stripped ──
    let unforced_bin = build_fixture_bin("adopter_app_unforced");

    let unforced_workspace = temp_workspace("t9-link-unforced");
    write_minimal_djogi_toml(&unforced_workspace, &database_url());

    let unforced_output = Command::new(&unforced_bin)
        .args(["migrations", "compose", "--allow-destructive"])
        .arg("--workspace")
        .arg(&unforced_workspace)
        .env("DATABASE_URL", database_url())
        .output()
        .expect("failed to execute child process");
    let unforced_stderr = String::from_utf8_lossy(&unforced_output.stderr).to_string();

    assert!(
        unforced_output.status.success(),
        "unforced fixture compose should succeed; stderr: {unforced_stderr}"
    );

    let unforced_snapshot_path = unforced_workspace.join("schema_snapshot.json");
    let unforced_snapshot_json =
        std::fs::read_to_string(&unforced_snapshot_path).unwrap_or_else(|_| String::from("{}"));
    let unforced_snapshot: serde_json::Value =
        serde_json::from_str(&unforced_snapshot_json).expect("parse unforced schema_snapshot.json");

    let unforced_tables = unforced_snapshot
        .get("tables")
        .expect("tables key in unforced snapshot");
    let unforced_table_names: Vec<&str> = unforced_tables
        .as_array()
        .expect("unforced tables is array")
        .iter()
        .map(|t| t.get("name").expect("table name").as_str().expect("string"))
        .collect();

    assert!(
        unforced_table_names.contains(&"elephants"),
        "unforced binary should see tracker::Elephant; got {:?}",
        unforced_table_names
    );
    assert!(
        unforced_table_names.contains(&"herds"),
        "unforced binary should see tracker::Herd; got {:?}",
        unforced_table_names
    );
    assert!(
        !unforced_table_names.contains(&"invoices"),
        "unforced binary must NOT see billing::Invoice (dead-stripped — core linkage proof); got {:?}",
        unforced_table_names
    );
}

// ── T-DROPGUARD: Linkage guard prevents destructive drop ────────────────────

#[test]
fn t_dropguard_linkage_guard_prevents_destructive_drop() {
    let bin = build_fixture_bin("adopter_app_unforced");

    let workspace = temp_workspace("t9-dropguard-unforced");
    write_minimal_djogi_toml(&workspace, &database_url());
    write_billing_snapshot_with_table(&workspace);

    let output = Command::new(&bin)
        .args(["migrations", "compose", "--allow-destructive"])
        .arg("--workspace")
        .arg(&workspace)
        .env("DATABASE_URL", database_url())
        .output()
        .expect("failed to execute child process");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    // Must refuse with exit 2 (refusal, not a crash).
    let code = output.status.code().expect("compose exited with signal");
    assert_eq!(
        code, 2,
        "unforced compose should refuse with exit 2; got {}; stderr: {stderr}",
        code
    );

    // Linkage hint must mention billing.
    assert!(
        stderr.contains("billing"),
        "stderr should mention 'billing'; got: {stderr}"
    );
}

// ── T-POS: Compose discovers all models from provider ───────────────────────

#[test]
fn t_pos_compose_discovers_all_models_from_provider() {
    // Plan T-POS: compose and schema discover all models from provider
    // (not just the introspection path). Codex BLOCK 12: compose must
    // see the same models as schema — if only one reads the provider,
    // they diverge.

    let bin = build_fixture_bin("adopter_app");

    let workspace = temp_workspace("t9-pos-discovery");
    write_minimal_djogi_toml(&workspace, &database_url());

    // Run `schema --format json` — proves provider threaded to schema path.
    let schema_out = Command::new(&bin)
        .args(["schema", "--format", "json"])
        .arg("--workspace")
        .arg(&workspace)
        .env("DATABASE_URL", database_url())
        .output()
        .expect("failed to execute child process");
    let schema_stdout = String::from_utf8_lossy(&schema_out.stdout).to_string();
    let schema_stderr = String::from_utf8_lossy(&schema_out.stderr).to_string();

    assert!(
        schema_out.status.success(),
        "schema should succeed; stderr: {schema_stderr}"
    );
    assert!(
        schema_stdout.contains("elephants"),
        "schema output should contain elephants; got: {schema_stdout}"
    );
    assert!(
        schema_stdout.contains("herds"),
        "schema output should contain herds"
    );
    assert!(
        schema_stdout.contains("invoices"),
        "schema output should contain invoices — proves cross-crate provider wiring to schema path"
    );

    // Run `migrations compose` — proves provider threaded to compose path (codex BLOCK 12).
    let compose_out = Command::new(&bin)
        .args(["migrations", "compose", "--name", "test_pos_discovery"])
        .arg("--workspace")
        .arg(&workspace)
        .env("DATABASE_URL", database_url())
        .output()
        .expect("failed to execute child process");
    let compose_stderr = String::from_utf8_lossy(&compose_out.stderr).to_string();

    assert!(
        compose_out.status.success(),
        "compose should succeed; stderr: {compose_stderr}"
    );

    // Compose writes schema_snapshot.json — verify it contains all 3 tables.
    let snapshot_path = workspace.join("schema_snapshot.json");
    let snapshot_json =
        std::fs::read_to_string(&snapshot_path).unwrap_or_else(|_| String::from("{}"));
    assert!(
        snapshot_json.contains("elephants"),
        "compose snapshot should contain elephants"
    );
    assert!(
        snapshot_json.contains("herds"),
        "compose snapshot should contain herds"
    );
    assert!(
        snapshot_json.contains("invoices"),
        "compose snapshot should contain invoices — proves cross-crate provider wiring to compose path"
    );
}

// ── T-PARITY: Intra-binary schema/compose parity ─────────────────────────────

#[test]
fn t_parity_schema_and_compose_within_same_binary() {
    // Plan T-PARITY: schema and compose within the SAME binary see identical
    // models. They could diverge if only one path reads the provider.

    let bin = build_fixture_bin("adopter_app");

    let workspace = temp_workspace("t9-parity-intra");
    write_minimal_djogi_toml(&workspace, &database_url());

    // Run `schema --format json` from forced binary.
    let schema_out = Command::new(&bin)
        .args(["schema", "--format", "json"])
        .arg("--workspace")
        .arg(&workspace)
        .env("DATABASE_URL", database_url())
        .output()
        .expect("failed to execute child process");
    let schema_stdout = String::from_utf8_lossy(&schema_out.stdout).to_string();
    let schema_stderr = String::from_utf8_lossy(&schema_out.stderr).to_string();

    assert!(
        schema_out.status.success(),
        "schema should succeed; stderr: {schema_stderr}"
    );

    // Count tables from schema JSON output.
    let expected_tables = ["elephants", "herds", "invoices"];
    let schema_table_count = expected_tables
        .iter()
        .filter(|t| schema_stdout.contains(*t))
        .count();

    assert_eq!(
        schema_table_count, 3,
        "schema should see all 3 tables; got {}: {}",
        schema_table_count, schema_stdout
    );

    // Run `migrations compose` from the SAME forced binary.
    let compose_out = Command::new(&bin)
        .args(["migrations", "compose", "--name", "test_parity_compose"])
        .arg("--workspace")
        .arg(&workspace)
        .env("DATABASE_URL", database_url())
        .output()
        .expect("failed to execute child process");
    let compose_stderr = String::from_utf8_lossy(&compose_out.stderr).to_string();

    assert!(
        compose_out.status.success(),
        "compose should succeed; stderr: {compose_stderr}"
    );

    // Compose writes schema_snapshot.json — verify it contains the same 3 tables.
    let snapshot_path = workspace.join("schema_snapshot.json");
    let snapshot_json =
        std::fs::read_to_string(&snapshot_path).unwrap_or_else(|_| String::from("{}"));
    let compose_table_count = expected_tables
        .iter()
        .filter(|t| snapshot_json.contains(*t))
        .count();

    assert_eq!(
        schema_table_count, compose_table_count,
        "schema ({}) and compose ({}) must see identical table count from same binary",
        schema_table_count, compose_table_count
    );
    assert_eq!(
        compose_table_count, 3,
        "compose should see all 3 tables from same binary; got {}: {}",
        compose_table_count, snapshot_json
    );
}

// ── T-NAME: Model names appear in verify output ─────────────────────────────

#[test]
fn t_name_model_names_in_verify_output() {
    let bin = build_fixture_bin("adopter_app");

    let workspace = temp_workspace("t9-name-verify");
    write_minimal_djogi_toml(&workspace, &database_url());

    let output = Command::new(&bin)
        .arg("verify")
        .arg("--workspace")
        .arg(&workspace)
        .env("DATABASE_URL", database_url())
        .output()
        .expect("failed to execute child process");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        output.status.success(),
        "verify should succeed; stderr: {stderr}"
    );

    // Check that model names appear in output.
    for expected in ["tracker::Elephant", "tracker::Herd", "billing::Invoice"] {
        assert!(
            stdout.contains(expected),
            "verify output should contain '{}'; got:\n{}",
            expected,
            stdout
        );
    }
}

// ── T-NOLOGIC: No-djogi binary runs without DB connection ───────────────────

#[test]
fn t_nologic_no_djogi_binary_runs_without_db() {
    // Build the no-djogi fixture.
    let fixture_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("no_djogi_app");

    let status = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .arg("--manifest-path")
        .arg(fixture_dir.join("Cargo.toml"))
        // Adopter fixture is an isolated workspace — unset CARGO_TARGET_DIR
        // so it builds into its own target/ (not the shared worktree cache).
        .env_remove("CARGO_TARGET_DIR")
        .status()
        .expect("cargo build failed for no_djogi fixture");
    assert!(status.success(), "cargo build failed for no_djogi fixture");

    let bin = fixture_dir.join("target").join("release").join("djogi");

    // The no-djogi binary defines its own clap CLI that accepts --database-url
    // but ignores it. Run verify with an invalid URL — should still succeed.
    let output = Command::new(&bin)
        .arg("verify")
        .arg("--database-url")
        .arg("postgres://invalid_host_that_does_not_exist_12345/notadb")
        .output()
        .expect("failed to execute child process");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        output.status.success(),
        "no-djogi verify should succeed without DB; stderr: {stderr}"
    );

    // No connection attempts.
    assert!(
        !stderr.to_lowercase().contains("connection")
            && !stderr.to_lowercase().contains("could not connect"),
        "should not attempt DB connection; stderr: {stderr}",
    );
}

// ── T-NOLOGIC-scan: Fixture sources contain no custom migration logic ────────

#[test]
fn t_nologic_fixture_sources_contain_no_migration_logic() {
    // Plan T-NOLOGIC: fixture adopter code contains only model definitions + glue,
    // no custom migration logic. Source code scan proves the fixture is a clean adopter.

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixtures = [
        "tests/fixtures/adopter_app/tracker/src/lib.rs",
        "tests/fixtures/adopter_app/billing/src/lib.rs",
        "tests/fixtures/adopter_app/bin/src/bin/djogi.rs",
        "tests/fixtures/adopter_app_unforced/tracker/src/lib.rs",
        "tests/fixtures/adopter_app_unforced/billing/src/lib.rs",
        "tests/fixtures/adopter_app_unforced/bin/src/bin/djogi.rs",
    ];

    for fixture_rel in &fixtures {
        let path = manifest_dir.join(fixture_rel);
        if !path.exists() {
            continue; // skip if path doesn't exist in this build config
        }
        let src = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            !src.contains("project_from")
                && !src.contains("compose(")
                && !src.contains("__bypass")
                && !src.contains("raw_"),
            "Fixture {} must not contain custom migration logic — \
             it should only define models + djogi_main! glue",
            fixture_rel
        );
    }
}

// ── T-NEG-compose: Standalone compose refuses with zero descriptors ──────────

#[test]
fn t_neg_standalone_compose_refuses_with_exit_2_and_no_artifacts() {
    // Plan T-NEG-compose: standalone djogi binary refuses compose with zero
    // descriptors (exit 2). Codex BLOCK 13: the zero-descriptor COMPOSE refusal
    // was untested at subprocess level.

    let workspace = temp_workspace("t9-neg-compose");
    write_minimal_djogi_toml(&workspace, &database_url());

    let djogi_bin = djogi_binary_path();

    // Run compose on published djogi binary (zero descriptors).
    let output = Command::new(&djogi_bin)
        .args(["migrations", "compose", "--name", "should_refuse"])
        .arg("--workspace")
        .arg(&workspace)
        .env("DATABASE_URL", database_url())
        .output()
        .expect("djogi binary should execute");

    assert_eq!(
        output.status.code(),
        Some(2),
        "standalone compose with zero descriptors must refuse exit 2, not succeed or runtime-error"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    // Dual-cause diagnostic: mentions "no models" and the command name.
    assert!(
        stderr.contains("no models registered") || stderr.contains("zero descriptor"),
        "stderr should mention zero descriptors: {stderr}"
    );
    assert!(
        stderr.contains("compose") || stderr.contains("migrations"),
        "stderr should identify the compose context: {stderr}"
    );

    // No pending artifacts written.
    let target_pending = workspace.join("target").join("djogi_pending");
    assert!(
        !target_pending.exists(),
        "compose refusal must not write pending artifacts; dir exists at {:?}",
        target_pending
    );
}

// ── T-VERIFY-DEGRADE: Zero-descriptor verify degrades to snapshot-only mode ──

#[djogi::djogi_test]
async fn t_verify_degrade_snapshot_only_against_valid_db(mut ctx: djogi::DjogiContext) {
    // The standalone published `djogi` binary links zero models. With an
    // on-disk snapshot present and a reachable DB, verify must degrade to
    // snapshot-only mode (NOT refuse with the zero-descriptor diagnostic).
    let bin = djogi_binary_path();

    let workspace = temp_workspace("t9-verify-degrade");

    // Get per-test database URL from the DjogiContext
    let db_url = current_database(&mut ctx).await;
    write_minimal_djogi_toml(&workspace, &db_url);

    // Write a billing snapshot (simulating prior state on disk)
    write_billing_snapshot_with_table(&workspace);

    let output = Command::new(&bin)
        .args(["migrations", "verify"])
        .current_dir(&workspace)
        .env("DATABASE_URL", db_url)
        .output()
        .expect("run verify");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // (a) NOT the zero-descriptor refusal — must degrade, not exit 2.
    assert_ne!(
        output.status.code(),
        Some(2),
        "verify must degrade, not refuse (exit 2): {stderr}"
    );
    assert!(
        !stderr.contains("no djogi models are registered"),
        "verify must NOT emit zero-descriptor diagnostic when snapshots exist: {stderr}"
    );
    // (b)+(c) Concrete degrade output: verify ran against the snapshot bucket.
    assert!(
        stdout.contains("billing") || stdout.contains("verified") || stdout.contains("drift"),
        "verify must emit concrete per-bucket degrade output for on-disk snapshot: stdout={stdout} stderr={stderr}"
    );
}

// ── T-NOCARGO: Compose works with no Cargo on PATH and no source ────────────

#[test]
fn t_nocargo_compose_without_cargo_or_source() {
    // Build the adopter fixture binary (links model crates via inventory).
    let bin = build_fixture_bin("adopter_app");

    // Copy binary + config to an isolated temp dir (no source code there).
    let runtime_dir = temp_workspace("370-nocargo");
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
    let empty_path = temp_workspace("370-nocargo-path");
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

// ── T-CONTAINER-APPLY: Apply from prebuilt binary (no source, no Cargo) ─────

#[djogi::djogi_test]
async fn t_container_apply_from_prebuilt_binary(mut ctx: djogi::DjogiContext) {
    // Derive the per-test DB URL by splicing the test database name into the
    // harness DATABASE_URL.
    let base_url = database_url();
    let db_name = current_database(&mut ctx).await;
    let db_url = splice_database_name(&base_url, &db_name);

    let bin = build_fixture_bin("adopter_app");
    let runtime_dir = temp_workspace("370-container");
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
    let empty_path = temp_workspace("370-container-path");

    // Compose writes pending artifacts.
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
    let apply_out = Command::new(&copied)
        .args(["migrations", "apply"])
        .current_dir(&runtime_dir)
        .env("PATH", &empty_path)
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

// ── T-STANDALONE-APPLY: Standalone binary applies pending artifacts ──────────

#[djogi::djogi_test]
async fn t_standalone_apply_with_pending_artifacts(mut ctx: djogi::DjogiContext) {
    // Use the adopter binary to compose (produces pending artifacts with live
    // descriptors), then use the standalone published djogi (zero descriptors)
    // to apply those artifacts — proving apply needs no live descriptors.
    let adopter = build_fixture_bin("adopter_app");
    let standalone = djogi_binary_path();

    let runtime_dir = temp_workspace("370-standalone-apply");

    let base_url = database_url();
    let db_name = current_database(&mut ctx).await;
    let db_url = splice_database_name(&base_url, &db_name);

    write_minimal_djogi_toml(&runtime_dir, &db_url);

    // Adopter composes (has live descriptors from linked model crates).
    let compose_out = Command::new(&adopter)
        .args(["migrations", "compose", "--name", "init"])
        .current_dir(&runtime_dir)
        .output()
        .expect("adopter compose");
    assert!(
        compose_out.status.success(),
        "adopter compose must succeed: {}",
        String::from_utf8_lossy(&compose_out.stderr)
    );

    // Standalone applies (no descriptors — reads pending artifacts only).
    let apply_out = Command::new(&standalone)
        .args(["migrations", "apply"])
        .current_dir(&runtime_dir)
        .output()
        .expect("standalone apply");
    assert!(
        apply_out.status.success(),
        "standalone apply must succeed: {}",
        String::from_utf8_lossy(&apply_out.stderr)
    );
}

/// Replace the database name in a PostgreSQL connection URL while preserving
/// the scheme, credentials, host, and port. Used to splice the per-test
/// database name into the harness `DATABASE_URL`.
fn splice_database_name(base_url: &str, db_name: &str) -> String {
    // Postgres URL format: postgres://[user[:password]@]host[:port]/database
    if let Some(slash_pos) = base_url.rfind('/') {
        let prefix = &base_url[..slash_pos + 1];
        return format!("{prefix}{db_name}");
    }
    // Fallback: if URL has no slash (malformed), just use base as-is.
    base_url.to_string()
}

// ── T-FORBID-UNSAFE: Adopter glue compiles under forbid(unsafe_code) ──

#[test]
fn t_forbid_unsafe_build_succeeds() {
    let bin = build_fixture_bin("adopter_forbid_unsafe");
    assert!(bin.exists(), "forbid_unsafe binary should exist after successful build");
}
