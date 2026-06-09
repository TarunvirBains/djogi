// `djogi verify` CLI end-to-end integration tests.
//
// # What these tests cover
//
// Two scenarios driven against the compiled `djogi` binary:
//
// 1. **Clean workspace.** Snapshot file on disk, matching audit row
//    on the audit DB, signing key unset (no-op sentinel) → `djogi
//    verify` exits `0` and prints `OK <path>` to stdout.
// 2. **Tampered snapshot.** Audit row written, then the snapshot
//    file is mutated → `djogi verify` exits `1` and prints
//    `MISMATCH <path>: ...` to stderr.
//
// # Single-DB simplification (vs. the spec's two-DB model)
//
// The production three-database architecture splits `crud_log_url`
// out from `database.url` so `djogi db reset` cannot erase the
// audit trail. The `#[djogi_test]` harness provisions ONE per-test
// database; provisioning a sibling audit DB would require either a
// second admin URL (operationally fragile) or a harness extension.
//
// The test works around this by pointing both the application URL and the
// `DJOGI_CRUD_LOG_URL` override at the SAME per-test database.
// `djogi_ddl_audit` is a namespaced table inside that DB, so the
// verify-vs-audit cross-check still runs end-to-end with realistic
// SQL. This is intentionally single-DB coverage; production
// `crud_log_url` separation remains a topology concern outside this
// subprocess test.
//
// # Shared CLI helpers
//
// `djogi_binary_path`, `current_database`, `temp_workspace`, and
// `write_minimal_djogi_toml` live in `djogi::testing::cli` (gated behind
// the `testing` feature) so this file and its sibling
// `djogi_analyze_recommendations.rs` share a single
// implementation. See djogi#119.
//
// # Spec / memory anchors
//
// - v3 plan §452, §459–460, §470, §824 — verify CLI semantics.
// - Plan specs for verify CLI semantics.
// - `djogi-cli/src/verify.rs` — the implementation under test.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use djogi::snapshot::sign::sign_snapshot;
use djogi::testing::cli::{
    current_database, djogi_binary_path, temp_workspace, write_minimal_djogi_toml,
};

fn test_database_url(database: &str) -> String {
    let admin_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    splice_db_into_url(&admin_url, database)
}

fn splice_db_into_url(url: &str, new_db: &str) -> String {
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("postgres://") {
        ("postgres://", rest)
    } else if let Some(rest) = url.strip_prefix("postgresql://") {
        ("postgresql://", rest)
    } else {
        panic!("DATABASE_URL must be a postgres:// or postgresql:// URL, got {url}");
    };

    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let tail = &rest[authority_end..];
    let query = tail.find('?').map_or("", |idx| &tail[idx..]);

    format!("{scheme}{authority}/{new_db}{query}")
}

/// Build the `migrations/<database>/<app>/` directory tree and write
/// a fixture `schema_snapshot.json` whose content the test will
/// later cross-check against the audit row.
///
/// Returns the absolute path to the snapshot file and its byte
/// content.
fn write_fixture_snapshot(workspace: &Path, database: &str, app: &str) -> (PathBuf, Vec<u8>) {
    let app_dir = if app.is_empty() { "_global_" } else { app };
    let dir = workspace.join("migrations").join(database).join(app_dir);
    fs::create_dir_all(&dir).expect("create migrations subtree");

    // Realistic minimal-snapshot shape — matches what the runner's
    // snapshot writer produces. The exact bytes do not matter for
    // the verify path (HMAC operates on raw bytes), but using a
    // realistic shape keeps the test useful as documentation.
    let payload = br#"{
  "version": 1,
  "models": []
}
"#
    .to_vec();
    let path = dir.join("schema_snapshot.json");
    fs::write(&path, &payload).expect("write schema_snapshot.json");
    (path, payload)
}

/// Insert one `djogi_ddl_audit` row tying `(database, app)` to the
/// supplied uppercase-hex signature. Bootstraps the audit table
/// idempotently first — the verify CLI is read-only and refuses to
/// create the table itself, so the test must do it.
async fn seed_audit_row(
    ctx: &mut djogi::DjogiContext,
    database: &str,
    app: &str,
    signature_hex: Option<&str>,
) {
    djogi::migrate::audit::bootstrap_ddl_audit(ctx)
        .await
        .expect("bootstrap_ddl_audit");
    djogi::migrate::audit::record_ddl(
        ctx,
        database,
        app,
        "-- verify fixture DDL",
        signature_hex,
    )
    .await
    .expect("record_ddl");
}

#[djogi::djogi_test]
async fn verify_clean_workspace_exits_zero(mut ctx: djogi::DjogiContext) {
    let database = current_database(&mut ctx).await;
    let app = ""; // global bucket — `_global_/` on disk

    // Workspace setup: temp dir + minimal Djogi.toml + snapshot file.
    let workspace = temp_workspace("verify-clean");
    let (snapshot_path, snapshot_bytes) = write_fixture_snapshot(&workspace, &database, app);

    // Build the per-test URL the test context is bound to by preserving
    // the harness DATABASE_URL authority/options and replacing only the
    // database name with `current_database()`.
    let test_url = test_database_url(&database);
    write_minimal_djogi_toml(&workspace, &test_url);

    // Compute the signature under the no-op key (env var unset);
    // hex-encode using the same path the runner does so the audit
    // row matches what verify will compute.
    let sig = sign_snapshot(&snapshot_bytes, &[0u8; 32]);
    let sig_hex = djogi::migrate::audit::signature_to_hex(&sig);
    seed_audit_row(&mut ctx, &database, app, Some(&sig_hex)).await;

    // Run `djogi verify --workspace <tmp>`. Override the audit DB
    // URL so it points at the same per-test DB (single-DB
    // simplification — see the module-level comment).
    let bin = djogi_binary_path();
    assert!(
        bin.is_file(),
        "djogi binary not found at {} — run `cargo build -p djogi-cli` first",
        bin.display(),
    );
    let output = Command::new(&bin)
        .arg("verify")
        .arg("--workspace")
        .arg(&workspace)
        .env("DATABASE_URL", &test_url)
        .env("DJOGI_CRUD_LOG_URL", &test_url)
        // Ensure we test the no-op key path (matches the audit row
        // we wrote, which used the no-op sentinel signature).
        .env_remove("DJOGI_SNAPSHOT_SIGNING_KEY")
        .output()
        .expect("spawn djogi verify");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected exit 0 on clean workspace; got {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code(),
    );
    assert!(
        stdout.contains(&format!("OK {}", snapshot_path.display())),
        "expected `OK <snapshot path>` on stdout; got\nstdout: {stdout}\nstderr: {stderr}",
    );

    // Cleanup the workspace; the per-test DB is dropped by the
    // `#[djogi_test]` harness automatically.
    let _ = fs::remove_dir_all(&workspace);
}

#[djogi::djogi_test]
async fn verify_mismatched_snapshot_exits_one(mut ctx: djogi::DjogiContext) {
    let database = current_database(&mut ctx).await;
    let app = "";

    let workspace = temp_workspace("verify-mismatch");
    let (snapshot_path, snapshot_bytes) = write_fixture_snapshot(&workspace, &database, app);

    let test_url = test_database_url(&database);
    write_minimal_djogi_toml(&workspace, &test_url);

    // We must use a NON-NO-OP signing key for this test. Under the
    // no-op key (`[0u8; 32]`), every signature is `[0u8; 32]` —
    // including the signature over a tampered payload — so the
    // verify path would report `OK` even after the tamper. The
    // tamper-detection contract only holds when the operator has
    // configured a real key. We reproduce that configuration here:
    // a fixed test-only HMAC key written to the env var the runner
    // and verifier both consult.
    //
    // The hex below decodes to `[0x01u8; 32]` — a low-entropy
    // fixture key chosen for stability across runs; production
    // adopters MUST use a random 32-byte key.
    let signing_key = [0x01u8; 32];
    let signing_key_hex = "0101010101010101010101010101010101010101010101010101010101010101";

    // Step 1 — write the audit row whose signature matches the
    // ORIGINAL bytes under the test-only signing key.
    let sig = sign_snapshot(&snapshot_bytes, &signing_key);
    let sig_hex = djogi::migrate::audit::signature_to_hex(&sig);
    seed_audit_row(&mut ctx, &database, app, Some(&sig_hex)).await;

    // Regression for issue #118: `db reset` writes audit rows with
    // `snapshot_signature_hex = NULL` because reset replays DDL without
    // persisting a fresh snapshot. Verify must ignore those NULL rows
    // when searching for the latest signed snapshot row; otherwise a
    // reset after the signed apply would mask this tamper mismatch and
    // downgrade it to a skip.
    seed_audit_row(&mut ctx, &database, app, None).await;

    // Step 2 — tamper the on-disk snapshot AFTER the audit row was
    // written. This simulates the canonical filesystem-tamper
    // attack.
    let mut tampered = snapshot_bytes.clone();
    tampered[0] ^= 0x01;
    fs::write(&snapshot_path, &tampered).expect("re-write tampered snapshot");

    // Step 3 — run `djogi verify` and assert exit 1 +
    // `MISMATCH <path>` on stderr.
    let bin = djogi_binary_path();
    assert!(
        bin.is_file(),
        "djogi binary not found at {} — run `cargo build -p djogi-cli` first",
        bin.display(),
    );
    let output = Command::new(&bin)
        .arg("verify")
        .arg("--workspace")
        .arg(&workspace)
        .env("DATABASE_URL", &test_url)
        .env("DJOGI_CRUD_LOG_URL", &test_url)
        // Match the key the test signed under so the verifier
        // recomputes the SAME HMAC the audit row carries.
        .env("DJOGI_SNAPSHOT_SIGNING_KEY", signing_key_hex)
        .output()
        .expect("spawn djogi verify");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1 on mismatch; got {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code(),
    );
    let mismatch_marker = format!("MISMATCH {}", snapshot_path.display());
    assert!(
        stderr.contains(&mismatch_marker),
        "expected `MISMATCH <snapshot path>` on stderr; got\nstdout: {stdout}\nstderr: {stderr}",
    );

    let _ = fs::remove_dir_all(&workspace);
}
