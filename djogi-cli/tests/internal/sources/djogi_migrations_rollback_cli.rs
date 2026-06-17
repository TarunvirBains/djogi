use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use djogi::config::MigrateConfig;
use djogi::migrate::{
    BucketKey, Classification, DriftBaseline, MigrationPlan, OperationSql, ResetSqlSide, RunnerCtx,
    RunnerIdentity, Segment, SegmentKind, WorkspaceGuard, acquire_workspace_lock, app_dirname,
    apply_plan, bucket_dir, compute_committed_down_sql_checksum, compute_committed_sql_checksum,
    down_filename, up_filename,
};
use djogi::testing::cli::{
    current_database, djogi_binary_path, temp_workspace, write_minimal_djogi_toml,
};

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

fn test_database_url(database: &str) -> String {
    let admin_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    splice_db_into_url(&admin_url, database)
}

fn temp_lock(workspace: &Path) -> PathBuf {
    workspace.join(".rollback-cli-test.lock")
}

fn acquire_test_workspace_guard(workspace: &Path) -> WorkspaceGuard {
    acquire_workspace_lock(&temp_lock(workspace), Duration::from_secs(2))
        .expect("acquire workspace lock")
}

fn single_statement_plan(bucket: &BucketKey, label: &str, up_sql: &str, down_sql: &str) -> MigrationPlan {
    MigrationPlan {
        bucket: bucket.clone(),
        classification: Classification::Additive,
        segments: vec![Segment {
            kind: SegmentKind::Transactional,
            statements: vec![OperationSql {
                label: label.to_string(),
                up: up_sql.to_string(),
                down: down_sql.to_string(),
                lossy: None,
            }],
        }],
    }
}

fn committed_runner_ctx(bucket: &BucketKey, version: &str, up_sql: &str, down_sql: &str) -> RunnerCtx {
    RunnerCtx {
        bucket: bucket.clone(),
        version: version.to_string(),
        description: format!("cli rollback fixture {version}"),
        checksum_up: compute_committed_sql_checksum(up_sql, ResetSqlSide::Up),
        checksum_down: compute_committed_down_sql_checksum(down_sql),
        snapshot: None,
        snapshot_path: None,
        config: MigrateConfig::default(),
        out_of_order_policy: djogi::migrate::OutOfOrderPolicy::AllowWithDiagnostic,
        audit_pool: None,
        runner_identity: Some(RunnerIdentity::SingleNodeDev),
        // rollback_plan never reads the apply-time drift baseline.
        drift_baseline: DriftBaseline::Disabled,
    }
}

fn write_committed_sql(
    workspace: &Path,
    bucket: &BucketKey,
    version: &str,
    up_sql: &str,
    down_sql: &str,
) {
    let workspace_canon = workspace
        .canonicalize()
        .expect("canonicalize workspace");
    let dir = bucket_dir(&workspace_canon, bucket);
    fs::create_dir_all(&dir).expect("create bucket dir");
    let dir = dir.canonicalize().expect("canonicalize bucket dir");
    assert!(dir.starts_with(&workspace_canon), "bucket dir escapes workspace");
    let up_path = dir.join(up_filename(version));
    let down_path = dir.join(down_filename(version));
    fs::write(up_path, up_sql).expect("write up sql");
    fs::write(down_path, down_sql).expect("write down sql");
}

fn safe_remove_workspace(workspace: &Path) {
    let workspace_canon = workspace
        .canonicalize()
        .expect("canonicalize workspace for cleanup");
    let temp_canon = std::env::temp_dir()
        .canonicalize()
        .expect("canonicalize temp dir");
    assert!(
        workspace_canon.starts_with(&temp_canon),
        "workspace cleanup escapes temp dir"
    );
    let _ = fs::remove_dir_all(&workspace_canon);
}

fn snapshot_path(workspace: &Path, bucket: &BucketKey) -> PathBuf {
    workspace
        .join("migrations")
        .join(&bucket.database)
        .join(app_dirname(&bucket.app))
        .join("schema_snapshot.json")
}

async fn table_exists(ctx: &mut djogi::DjogiContext, table_name: &str) -> bool {
    ctx.raw_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = $1 AND relkind = 'r')",
        &[&table_name],
    )
    .await
    .expect("table exists probe")
}

async fn ledger_status(ctx: &mut djogi::DjogiContext, version: &str) -> String {
    ctx.raw_scalar(
        "SELECT status::text FROM djogi_schema_migrations WHERE version = $1 AND app_label = ''",
        &[&version],
    )
    .await
    .expect("ledger status")
}

async fn ledger_note(ctx: &mut djogi::DjogiContext, version: &str) -> Option<String> {
    ctx.raw_scalar(
        "SELECT partial_apply_note FROM djogi_schema_migrations \
         WHERE version = $1 AND app_label = ''",
        &[&version],
    )
    .await
    .expect("ledger note")
}

fn spawn_rollback(workspace: &Path, database_url: &str, extra_args: &[&str]) -> Output {
    let bin = djogi_binary_path();
    assert!(
        bin.is_file(),
        "djogi binary not found at {} — run `cargo build -p djogi-cli` first",
        bin.display(),
    );

    let mut cmd = Command::new(&bin);
    cmd.arg("migrations").arg("rollback");
    cmd.args(extra_args);
    cmd.arg("--workspace").arg(workspace);
    cmd.env("DATABASE_URL", database_url);
    cmd.output().expect("spawn djogi migrations rollback")
}

async fn seed_applied_migration(
    ctx: &mut djogi::DjogiContext,
    workspace: &Path,
    version: &str,
    table_name: &str,
    up_sql: &str,
    down_sql: &str,
) -> (BucketKey, RunnerCtx) {
    let bucket = BucketKey {
        database: "main".to_string(),
        app: String::new(),
    };
    let plan = single_statement_plan(&bucket, table_name, up_sql, down_sql);
    let runner_ctx = committed_runner_ctx(&bucket, version, up_sql, down_sql);
    write_committed_sql(workspace, &bucket, version, up_sql, down_sql);

    let guard = acquire_test_workspace_guard(workspace);
    apply_plan(ctx, &plan, &runner_ctx, &guard)
        .await
        .expect("apply fixture migration");
    (bucket, runner_ctx)
}

#[djogi::djogi_test]
async fn rollback_success_rolls_back_and_reprojects_snapshot(mut ctx: djogi::DjogiContext) {
    let database = current_database(&mut ctx).await;
    let database_url = test_database_url(&database);
    let workspace = temp_workspace("rollback-cli-success");
    write_minimal_djogi_toml(&workspace, &database_url);

    let version = "V20260613010000__rollback_cli_success";
    let table_name = "rollback_cli_success";
    let up_sql = "CREATE TABLE \"rollback_cli_success\" (\"id\" BIGINT PRIMARY KEY);";
    let down_sql = "DROP TABLE \"rollback_cli_success\";";
    let (bucket, _) =
        seed_applied_migration(&mut ctx, &workspace, version, table_name, up_sql, down_sql).await;

    let output = spawn_rollback(&workspace, &database_url, &["--single-node-dev"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "expected exit 0\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("rolled back 1 migration(s); snapshot re-projected."),
        "missing success output\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !table_exists(&mut ctx, table_name).await,
        "rollback should drop the table"
    );
    assert_eq!(ledger_status(&mut ctx, version).await, "rolled_back");
    assert!(
        snapshot_path(&workspace, &bucket).is_file(),
        "snapshot rebuild should write schema_snapshot.json"
    );

    safe_remove_workspace(&workspace);
}

#[djogi::djogi_test]
async fn rollback_dry_run_previews_without_mutation(mut ctx: djogi::DjogiContext) {
    let database = current_database(&mut ctx).await;
    let database_url = test_database_url(&database);
    let workspace = temp_workspace("rollback-cli-dry-run");
    write_minimal_djogi_toml(&workspace, &database_url);

    let version = "V20260613010001__rollback_cli_dry_run";
    let table_name = "rollback_cli_dry_run";
    let up_sql = "CREATE TABLE \"rollback_cli_dry_run\" (\"id\" BIGINT PRIMARY KEY);";
    let down_sql = "DROP TABLE \"rollback_cli_dry_run\";";
    seed_applied_migration(&mut ctx, &workspace, version, table_name, up_sql, down_sql).await;

    let output = spawn_rollback(&workspace, &database_url, &["--dry-run"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "expected exit 0\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains(down_sql), "dry-run should print down SQL:\n{stdout}");
    assert!(stdout.contains("dry run"), "missing dry-run footer:\n{stdout}");
    assert!(
        table_exists(&mut ctx, table_name).await,
        "dry-run must not execute down SQL"
    );
    assert_eq!(ledger_status(&mut ctx, version).await, "applied");

    safe_remove_workspace(&workspace);
}

#[djogi::djogi_test]
async fn rollback_lossy_down_refuses_without_allow_data_loss(mut ctx: djogi::DjogiContext) {
    let database = current_database(&mut ctx).await;
    let database_url = test_database_url(&database);
    let workspace = temp_workspace("rollback-cli-lossy-refusal");
    write_minimal_djogi_toml(&workspace, &database_url);

    let version = "V20260613010002__rollback_cli_lossy_refusal";
    let table_name = "rollback_cli_lossy_refusal";
    let up_sql = "CREATE TABLE \"rollback_cli_lossy_refusal\" (\"id\" BIGINT PRIMARY KEY);";
    let down_sql =
        "-- LOSSY: DroppedTable — data is lost\nDROP TABLE \"rollback_cli_lossy_refusal\";";
    seed_applied_migration(&mut ctx, &workspace, version, table_name, up_sql, down_sql).await;

    let output = spawn_rollback(&workspace, &database_url, &["--single-node-dev"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected refusal exit 2\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("--allow-data-loss"),
        "lossy refusal should name opt-in flag\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        table_exists(&mut ctx, table_name).await,
        "refusal must not execute down SQL"
    );
    assert_eq!(ledger_status(&mut ctx, version).await, "applied");

    safe_remove_workspace(&workspace);
}

#[djogi::djogi_test]
async fn rollback_allow_data_loss_records_reason(mut ctx: djogi::DjogiContext) {
    let database = current_database(&mut ctx).await;
    let database_url = test_database_url(&database);
    let workspace = temp_workspace("rollback-cli-lossy-allow");
    write_minimal_djogi_toml(&workspace, &database_url);

    let version = "V20260613010003__rollback_cli_lossy_allow";
    let table_name = "rollback_cli_lossy_allow";
    let up_sql = "CREATE TABLE \"rollback_cli_lossy_allow\" (\"id\" BIGINT PRIMARY KEY);";
    let down_sql =
        "-- LOSSY: DroppedTable — data is lost\nDROP TABLE \"rollback_cli_lossy_allow\";";
    seed_applied_migration(&mut ctx, &workspace, version, table_name, up_sql, down_sql).await;

    let reason = "operator approved lossy rollback for fixture coverage";
    let output = spawn_rollback(
        &workspace,
        &database_url,
        &["--single-node-dev", "--allow-data-loss", "--reason", reason],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "expected exit 0\nstdout: {stdout}\nstderr: {stderr}"
    );
    let note = ledger_note(&mut ctx, version).await.expect("rolled_back note");
    assert!(
        note.contains(reason),
        "lossy reason should persist in ledger note: {note}"
    );
    assert!(
        !table_exists(&mut ctx, table_name).await,
        "lossy-allowed rollback should execute down SQL"
    );

    safe_remove_workspace(&workspace);
}

#[djogi::djogi_test]
async fn rollback_checksum_drift_refuses_and_executes_nothing(mut ctx: djogi::DjogiContext) {
    let database = current_database(&mut ctx).await;
    let database_url = test_database_url(&database);
    let workspace = temp_workspace("rollback-cli-checksum-drift");
    write_minimal_djogi_toml(&workspace, &database_url);

    let version = "V20260613010004__rollback_cli_checksum_drift";
    let table_name = "rollback_cli_checksum_drift";
    let up_sql = "CREATE TABLE \"rollback_cli_checksum_drift\" (\"id\" BIGINT PRIMARY KEY);";
    let down_sql = "DROP TABLE \"rollback_cli_checksum_drift\";";
    let (bucket, _) =
        seed_applied_migration(&mut ctx, &workspace, version, table_name, up_sql, down_sql).await;

    let down_path = bucket_dir(&workspace, &bucket).join(down_filename(version));
    let down_parent_canon = down_path
        .parent()
        .expect("down path parent")
        .canonicalize()
        .expect("canonicalize down path parent");
    let down_path = down_parent_canon.join(down_path.file_name().expect("down path file name"));
    fs::write(&down_path, format!("{down_sql}\nSELECT 1;\n")).expect("tamper down file");

    let output = spawn_rollback(&workspace, &database_url, &["--single-node-dev"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected checksum-drift refusal exit 2\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("checksum") && stderr.contains("repair checksum-drift"),
        "checksum refusal should name repair flow\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        table_exists(&mut ctx, table_name).await,
        "checksum drift refusal must execute nothing"
    );
    assert_eq!(ledger_status(&mut ctx, version).await, "applied");
    assert!(
        !stdout.contains("snapshot re-projected"),
        "pre-execution refusal must not trigger snapshot rebuild"
    );

    safe_remove_workspace(&workspace);
}

#[djogi::djogi_test]
async fn rollback_post_commit_failure_still_reprojects_snapshot(mut ctx: djogi::DjogiContext) {
    let database = current_database(&mut ctx).await;
    let database_url = test_database_url(&database);
    let workspace = temp_workspace("rollback-cli-post-commit");
    write_minimal_djogi_toml(&workspace, &database_url);

    let version = "V20260613010005__rollback_cli_post_commit";
    let table_name = "rollback_cli_post_commit";
    let up_sql = "CREATE TABLE \"rollback_cli_post_commit\" (\"id\" BIGINT PRIMARY KEY);";
    let down_sql =
        "DROP TABLE \"rollback_cli_post_commit\"; DROP TABLE djogi_schema_migrations;";
    let (bucket, _) =
        seed_applied_migration(&mut ctx, &workspace, version, table_name, up_sql, down_sql).await;

    let output = spawn_rollback(&workspace, &database_url, &["--single-node-dev"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected runtime exit 1\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("rollback failed at runner level"),
        "post-commit failure should surface runner error\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("snapshot re-projected to match committed rollback work."),
        "CLI must rebuild the snapshot even when the first target fails post-commit\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !table_exists(&mut ctx, table_name).await,
        "committed rollback should still drop the target table"
    );
    assert!(
        snapshot_path(&workspace, &bucket).is_file(),
        "post-commit failure path should still rebuild the snapshot"
    );

    safe_remove_workspace(&workspace);
}
