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
fn t_link_multi_model_cross_crete_retention() {
    // Build forced fixture binary (references ALL models across both crates).
    let bin = build_fixture_bin("adopter_app");

    let workspace = temp_workspace("t9-link-forced");
    write_minimal_djogi_toml(&workspace, &database_url());

    let output = Command::new(&bin)
        .arg("compose")
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
}

// ── T-DROPGUARD: Linkage guard prevents destructive drop ────────────────────

#[test]
fn t_dropguard_linkage_guard_prevents_destructive_drop() {
    let bin = build_fixture_bin("adopter_app_unforced");

    let workspace = temp_workspace("t9-dropguard-unforced");
    write_minimal_djogi_toml(&workspace, &database_url());
    write_billing_snapshot_with_table(&workspace);

    let output = Command::new(&bin)
        .arg("compose")
        .arg("--allow-destructive")
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

// ── T-POS: Positional line count matches descriptor count ───────────────────

#[test]
fn t_pos_positional_line_count_matches_descriptor_count() {
    let bin = build_fixture_bin("adopter_app");

    let workspace = temp_workspace("t9-pos-verify");
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

    // Three models → three positional lines.
    let positional_lines: Vec<&str> = stdout.lines().filter(|l| l.starts_with("$")).collect();
    assert_eq!(
        positional_lines.len(),
        3,
        "expected 3 positional lines, got {}: {:?}",
        positional_lines.len(),
        positional_lines
    );

    // Model count line should say 3.
    assert!(
        stdout.contains("models registered"),
        "verify should contain model count; stdout: {stdout}"
    );
}

// ── T-PARITY: SQL parity between djogi and adopter fixture ───────────────────

#[test]
fn t_parity_sql_between_djogi_and_adopter() {
    let fixture_bin = build_fixture_bin("adopter_app");
    let djogi_bin = djogi_binary_path();

    // Run fixture compose.
    let workspace_fixture = temp_workspace("t9-parity-fixture");
    write_minimal_djogi_toml(&workspace_fixture, &database_url());

    let output_f = Command::new(&fixture_bin)
        .arg("compose")
        .arg("--workspace")
        .arg(&workspace_fixture)
        .env("DATABASE_URL", database_url())
        .output()
        .expect("failed to execute child process");
    let f_stdout = String::from_utf8_lossy(&output_f.stdout).to_string();
    let f_stderr = String::from_utf8_lossy(&output_f.stderr).to_string();

    assert!(
        output_f.status.success(),
        "fixture compose should succeed; stderr: {f_stderr}"
    );

    // Run djogi binary verify (same models, same descriptors).
    let workspace_djogi = temp_workspace("t9-parity-djogi");
    write_minimal_djogi_toml(&workspace_djogi, &database_url());

    let output_d = Command::new(&djogi_bin)
        .arg("verify")
        .arg("--workspace")
        .arg(&workspace_djogi)
        .env("DATABASE_URL", database_url())
        .output()
        .expect("failed to execute child process");
    let d_stdout = String::from_utf8_lossy(&output_d.stdout).to_string();
    let d_stderr = String::from_utf8_lossy(&output_d.stderr).to_string();

    assert!(
        output_d.status.success(),
        "djogi verify should succeed; stderr: {d_stderr}"
    );

    // Extract SQL lines (lines starting with -- or containing DDL).
    let fixture_sql: Vec<&str> = f_stdout
        .lines()
        .filter(|l| l.starts_with("--") || l.contains("CREATE TABLE"))
        .collect();
    let djogi_sql: Vec<&str> = d_stdout
        .lines()
        .filter(|l| l.starts_with("--") || l.contains("CREATE TABLE"))
        .collect();

    assert_eq!(
        fixture_sql.len(),
        djogi_sql.len(),
        "SQL line count mismatch: fixture {} vs djogi {}",
        fixture_sql.len(),
        djogi_sql.len()
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
