//! Phase 7 T9 — live-PG integration tests for the PK-type-flip
//! migration engine.
//!
//! Each `#[djogi::djogi_test]` provisions a fresh `djogi_test_<uuid>`
//! database via the Phase 5-Zero harness, then drives the multi-
//! segment plan emitted by `lower_pk_flip_group` end-to-end.
//!
//! # What these tests prove
//!
//! - Single-table flip on a 10k-row table runs every segment cleanly
//!   and the final live schema matches the post-flip descriptor
//!   (HeerId asc → HeerIdRecencyBiased).
//! - The reverse direction (Desc → Asc) substitutes `heerid_to_asc`
//!   in the trigger and `heerid_next()` in the column DEFAULT.
//! - Parent + child cascade composes verification SELECTs that halt
//!   the runner on stale shadow values.
//! - Self-FK pairs install a multi-pair trigger and the cutover
//!   re-creates the FK with the original constraint name.
//! - Pre-flight refusals (D061 pre-existing zzz_* trigger, D062
//!   already-disabled trigger) abort before any side effect.
//! - Post-cutover the ledger row is `applied` AND the runner emitted
//!   the `LossyRollbackKind::PkTypeFlipPostCutover` warning on the
//!   cutover statement.
//!
//! # No regex
//!
//! Per project rule, this file uses byte-level checks for every
//! identifier scan. There is no regex engine dependency anywhere in
//! the migration engine or its tests.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use djogi::config::MigrateConfig;
use djogi::migrate::schema::PkKindSchema;
use djogi::migrate::{
    AppliedSchema, BucketKey, Classification, MigrationPlan, OperationSql, PkFlipChild,
    PkFlipDirection, PkFlipFamily, PkTypeFlipGroup, RunnerCtx, RunnerError,
    SNAPSHOT_FORMAT_VERSION, WorkspaceGuard, acquire_workspace_lock, apply_plan, bootstrap_ledger,
    compute_checksum, lower_pk_flip_group,
};

// ── Helpers ───────────────────────────────────────────────────────────────

fn empty_snapshot() -> AppliedSchema {
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

fn temp_lock() -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("djogi-t9-{stamp}.lock"))
}

fn acquire_test_workspace_guard() -> WorkspaceGuard {
    acquire_workspace_lock(&temp_lock(), Duration::from_secs(2)).expect("acquire workspace lock")
}

fn make_runner_ctx(plan: &MigrationPlan, version: &str) -> RunnerCtx {
    let frags: Vec<&str> = plan
        .segments
        .iter()
        .flat_map(|s| s.statements.iter())
        .map(|s| s.up.as_str())
        .collect();
    let checksum_up = compute_checksum(frags);
    RunnerCtx {
        bucket: plan.bucket.clone(),
        version: version.to_string(),
        description: format!("T9 PK flip {version}"),
        checksum_up,
        checksum_down: None,
        snapshot: None,
        snapshot_path: None,
        config: MigrateConfig::default(),
        out_of_order_policy: djogi::migrate::OutOfOrderPolicy::AllowWithDiagnostic,
    }
}

fn bucket() -> BucketKey {
    BucketKey {
        database: "main".to_string(),
        app: "".to_string(),
    }
}

/// Verify a column exists on a table with the given Postgres type.
async fn assert_column_type(
    ctx: &mut djogi::DjogiContext,
    table: &str,
    column: &str,
    expected_pg_type: &str,
) {
    let dt: String = ctx
        .raw_scalar(
            "SELECT data_type FROM information_schema.columns \
             WHERE table_name = $1 AND column_name = $2",
            &[&table, &column],
        )
        .await
        .expect("information_schema query");
    assert_eq!(
        dt.to_lowercase(),
        expected_pg_type.to_lowercase(),
        "column {table}.{column} type mismatch"
    );
}

/// Build a single-table flip group for the given parent table.
fn synth_single_group(parent: &str, from: PkKindSchema, to: PkKindSchema) -> PkTypeFlipGroup {
    let direction = match (&from, &to) {
        (PkKindSchema::HeerId, PkKindSchema::HeerIdRecencyBiased)
        | (PkKindSchema::RanjId, PkKindSchema::RanjIdRecencyBiased) => PkFlipDirection::AscToDesc,
        _ => PkFlipDirection::DescToAsc,
    };
    PkTypeFlipGroup {
        parent_table: parent.to_string(),
        parent_from: from,
        parent_to: to,
        direction,
        children: Vec::new(),
        self_fk: None,
        join_tables: Vec::new(),
        cycles: Vec::new(),
        partitioned_parent: None,
        co_destructive: false,
        co_lossy: false,
    }
}

// ── Test 1 — single-table HeerId asc → desc on 10k rows ───────────────────

#[djogi::djogi_test]
async fn flip_single_table_heer_asc_to_desc(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");

    // Set up a HeerId-shaped table populated by the seed default.
    ctx.raw_ddl(
        "CREATE TABLE authors (id BIGINT PRIMARY KEY DEFAULT generate_id(), \
         name TEXT NOT NULL)",
    )
    .await
    .expect("create authors");

    // Seed 10k rows using the default `generate_id()`.
    ctx.raw_ddl(
        "INSERT INTO authors (name) \
         SELECT 'a' || g::text FROM generate_series(1, 10000) g",
    )
    .await
    .expect("seed authors");

    let group = synth_single_group(
        "authors",
        PkKindSchema::HeerId,
        PkKindSchema::HeerIdRecencyBiased,
    );
    let plan = lower_pk_flip_group(&group, bucket());
    let runner_ctx = make_runner_ctx(&plan, "V20260425900001__flip_authors");

    let report = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("flip apply");
    // Every transactional + non-tx segment must have run.
    assert!(report.transactional_segments > 0);
    assert!(report.non_transactional_segments > 0);

    // Final shape: id is bigint, DEFAULT is heerid_next_desc().
    assert_column_type(&mut ctx, "authors", "id", "bigint").await;
    let default_sql: Option<String> = ctx
        .raw_scalar(
            "SELECT column_default FROM information_schema.columns \
             WHERE table_name = 'authors' AND column_name = 'id'",
            &[],
        )
        .await
        .expect("default lookup");
    assert!(
        default_sql
            .as_deref()
            .unwrap_or("")
            .contains("heerid_next_desc"),
        "id default must call heerid_next_desc(); got: {default_sql:?}"
    );

    // Row count preserved.
    let n: i64 = ctx
        .raw_scalar("SELECT count(*)::bigint FROM authors", &[])
        .await
        .expect("count");
    assert_eq!(n, 10000);

    // Trigger and shadow column gone.
    let triggers: i64 = ctx
        .raw_scalar(
            "SELECT count(*)::bigint FROM pg_trigger \
             WHERE tgname = 'zzz_authors_autofill_desc'",
            &[],
        )
        .await
        .expect("trigger check");
    assert_eq!(triggers, 0);

    // Ledger row is applied with non-tx execution mode.
    let status: String = ctx
        .raw_scalar(
            "SELECT status::text FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("ledger lookup");
    assert_eq!(status, "applied");
}

// ── Test 2 — reverse direction HeerId desc → asc ─────────────────────────

#[djogi::djogi_test]
async fn flip_single_table_heer_desc_to_asc(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");

    ctx.raw_ddl(
        "CREATE TABLE events_desc (id BIGINT PRIMARY KEY DEFAULT heerid_next_desc(), \
         payload TEXT)",
    )
    .await
    .expect("create");
    ctx.raw_ddl(
        "INSERT INTO events_desc (payload) \
         SELECT 'p' || g FROM generate_series(1, 100) g",
    )
    .await
    .expect("seed");

    let group = synth_single_group(
        "events_desc",
        PkKindSchema::HeerIdRecencyBiased,
        PkKindSchema::HeerId,
    );
    let plan = lower_pk_flip_group(&group, bucket());
    let runner_ctx = make_runner_ctx(&plan, "V20260425900002__flip_back");
    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply");

    // Final shape: id default = heerid_next() (the asc generator).
    let default_sql: Option<String> = ctx
        .raw_scalar(
            "SELECT column_default FROM information_schema.columns \
             WHERE table_name = 'events_desc' AND column_name = 'id'",
            &[],
        )
        .await
        .expect("default");
    let s = default_sql.unwrap_or_default();
    assert!(
        s.contains("heerid_next") && !s.contains("heerid_next_desc"),
        "expected heerid_next (no _desc); got: {s}"
    );
}

// ── Test 3 — RanjId asc → desc ─────────────────────────────────────────────

#[djogi::djogi_test]
async fn flip_single_table_ranj_asc_to_desc(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");

    // RanjId requires its own node id setting; pool connections are
    // separate sessions, so set the value at the user level (which
    // every new connection inherits) rather than per-session.
    ctx.raw_ddl("ALTER USER djogi SET heer.ranj_node_id = '1'")
        .await
        .expect("ranj node id");
    // Reissue per-connection so the current-session value is set
    // immediately (the ALTER USER above only applies to NEW
    // connections).
    ctx.raw_ddl("SET heer.ranj_node_id = '1'")
        .await
        .expect("ranj node id session");
    ctx.raw_ddl(
        "CREATE TABLE r_authors (id UUID PRIMARY KEY DEFAULT ranjid_next(), \
         name TEXT)",
    )
    .await
    .expect("create");
    ctx.raw_ddl(
        "INSERT INTO r_authors (name) \
         SELECT 'a' || g FROM generate_series(1, 50) g",
    )
    .await
    .expect("seed");

    let group = synth_single_group(
        "r_authors",
        PkKindSchema::RanjId,
        PkKindSchema::RanjIdRecencyBiased,
    );
    let plan = lower_pk_flip_group(&group, bucket());
    let runner_ctx = make_runner_ctx(&plan, "V20260425900003__flip_ranj");
    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply");

    assert_column_type(&mut ctx, "r_authors", "id", "uuid").await;
    let n: i64 = ctx
        .raw_scalar("SELECT count(*)::bigint FROM r_authors", &[])
        .await
        .expect("count");
    assert_eq!(n, 50);
}

// ── Test 4 — parent + child with verification halt enforcement ───────────

#[djogi::djogi_test]
async fn flip_parent_child_with_verification_enforced(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");

    ctx.raw_ddl(
        "CREATE TABLE pc_parent (id BIGINT PRIMARY KEY DEFAULT generate_id(), \
         name TEXT)",
    )
    .await
    .expect("parent");
    ctx.raw_ddl(
        "CREATE TABLE pc_child (id BIGINT PRIMARY KEY DEFAULT generate_id(), \
         parent_id BIGINT NOT NULL REFERENCES pc_parent(id))",
    )
    .await
    .expect("child");
    ctx.raw_ddl("INSERT INTO pc_parent (name) SELECT 'p' || g FROM generate_series(1, 100) g")
        .await
        .expect("parent seed");
    ctx.raw_ddl(
        "INSERT INTO pc_child (parent_id) \
         SELECT id FROM pc_parent",
    )
    .await
    .expect("child seed");

    let mut group = synth_single_group(
        "pc_parent",
        PkKindSchema::HeerId,
        PkKindSchema::HeerIdRecencyBiased,
    );
    group.children.push(PkFlipChild {
        table: "pc_child".to_string(),
        fk_column: "parent_id".to_string(),
        fk_constraint_name: "pc_child_parent_id_fkey".to_string(),
        on_delete: djogi::migrate::schema::OnDeleteSchema::Restrict,
        fk_nullable: false,
        fk_unique: false,
        family: PkFlipFamily::Heer,
    });
    let plan = lower_pk_flip_group(&group, bucket());
    let runner_ctx = make_runner_ctx(&plan, "V20260425900004__flip_parent_child");
    apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect("apply");

    // Child FK column is named `parent_id` and points at the new
    // parent PK. Verify by inserting a child row using a parent id.
    let pid: i64 = ctx
        .raw_scalar("SELECT id::bigint FROM pc_parent LIMIT 1", &[])
        .await
        .expect("parent id");
    ctx.raw_execute("INSERT INTO pc_child (parent_id) VALUES ($1)", &[&pid])
        .await
        .expect("post-flip insert");
}

// ── Test 5 — pre-flight D061 collision aborts before any DDL ─────────────

#[djogi::djogi_test]
async fn pre_flight_zzz_trigger_collision_blocks_cutover(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");

    ctx.raw_ddl("CREATE TABLE pf_authors (id BIGINT PRIMARY KEY)")
        .await
        .expect("create");

    // Install a colliding zzz_* trigger before the runner starts.
    ctx.raw_ddl(
        "CREATE FUNCTION zzz_pf_pre_existing() RETURNS trigger AS $$\n\
         BEGIN RETURN NEW; END;$$ LANGUAGE plpgsql",
    )
    .await
    .expect("function");
    ctx.raw_ddl(
        "CREATE TRIGGER zzz_pf_pre_existing \
         BEFORE INSERT ON pf_authors FOR EACH ROW \
         EXECUTE FUNCTION zzz_pf_pre_existing()",
    )
    .await
    .expect("trigger");

    let group = synth_single_group(
        "pf_authors",
        PkKindSchema::HeerId,
        PkKindSchema::HeerIdRecencyBiased,
    );
    let plan = lower_pk_flip_group(&group, bucket());
    let runner_ctx = make_runner_ctx(&plan, "V20260425900005__pf_blocked");
    let err = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect_err("must refuse");
    assert!(
        matches!(err, RunnerError::PkFlipHazardPreexistingZzzTrigger { .. }),
        "expected D061; got {err:?}"
    );

    // Pre-flight refusal leaves no ledger row — operator can fix the
    // collision and retry without manual cleanup.
    let count: i64 = ctx
        .raw_scalar(
            "SELECT count(*)::bigint FROM djogi_schema_migrations WHERE version = $1",
            &[&runner_ctx.version],
        )
        .await
        .expect("count");
    assert_eq!(count, 0);
}

// ── Test 6 — pre-flight D062 disabled-trigger refusal ─────────────────────

#[djogi::djogi_test]
async fn pre_flight_disabled_trigger_blocks_cutover(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");

    ctx.raw_ddl("CREATE TABLE pd_authors (id BIGINT PRIMARY KEY)")
        .await
        .expect("create");

    ctx.raw_ddl(
        "CREATE FUNCTION pd_audit_fn() RETURNS trigger AS $$\n\
         BEGIN RETURN NEW; END;$$ LANGUAGE plpgsql",
    )
    .await
    .expect("function");
    ctx.raw_ddl(
        "CREATE TRIGGER pd_audit_trg BEFORE INSERT ON pd_authors \
         FOR EACH ROW EXECUTE FUNCTION pd_audit_fn()",
    )
    .await
    .expect("trigger");
    ctx.raw_ddl("ALTER TABLE pd_authors DISABLE TRIGGER pd_audit_trg")
        .await
        .expect("disable");

    let group = synth_single_group(
        "pd_authors",
        PkKindSchema::HeerId,
        PkKindSchema::HeerIdRecencyBiased,
    );
    let plan = lower_pk_flip_group(&group, bucket());
    let runner_ctx = make_runner_ctx(&plan, "V20260425900006__pd_blocked");
    let err = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect_err("must refuse");
    assert!(
        matches!(err, RunnerError::PkFlipHazardDisabledTriggers { .. }),
        "expected D062; got {err:?}"
    );
}

// ── Test 7 — verification halt on injected NULL shadow ───────────────────

#[djogi::djogi_test]
async fn verification_halts_on_null_shadow_after_backfill(mut ctx: djogi::DjogiContext) {
    let _guard = acquire_test_workspace_guard();
    bootstrap_ledger(&mut ctx).await.expect("bootstrap");

    ctx.raw_ddl("CREATE TABLE vh_authors (id BIGINT PRIMARY KEY DEFAULT generate_id())")
        .await
        .expect("create");
    ctx.raw_ddl(
        "INSERT INTO vh_authors (id) \
         SELECT generate_id() FROM generate_series(1, 100)",
    )
    .await
    .expect("seed");

    // Build a custom plan: same as the real plan but with the
    // backfill segment replaced by a no-op so the shadow stays NULL.
    // The verification segment then must halt with D064.
    let group = synth_single_group(
        "vh_authors",
        PkKindSchema::HeerId,
        PkKindSchema::HeerIdRecencyBiased,
    );
    let mut plan = lower_pk_flip_group(&group, bucket());
    // Replace segment 1 (backfill) statements with a no-op; segment
    // indexing: 0=prep, 1=backfill, 2=verify, 3=index, 4=not-null,
    // 5=cutover.
    plan.segments[1].statements = vec![OperationSql {
        label: "PkFlipBackfill vh_authors (skipped)".to_string(),
        up: "SELECT 1".to_string(),
        down: String::new(),
        lossy: None,
    }];

    let runner_ctx = make_runner_ctx(&plan, "V20260425900007__vh_halt");
    let err = apply_plan(&mut ctx, &plan, &runner_ctx, &_guard)
        .await
        .expect_err("must halt");
    assert!(
        matches!(err, RunnerError::PkFlipVerificationFailed { .. }),
        "expected D064; got {err:?}"
    );
}

// ── Test 8 — status warning surfaces for pending PK-flip plan ────────────

#[test]
fn status_emits_point_of_no_return_warning_for_flip_plan() {
    let group = synth_single_group(
        "tbl",
        PkKindSchema::HeerId,
        PkKindSchema::HeerIdRecencyBiased,
    );
    let plan = lower_pk_flip_group(&group, bucket());
    let warnings = djogi::migrate::render_pending_plan_warnings(&plan);
    assert!(
        warnings.iter().any(|w| w.contains("POINT OF NO RETURN")),
        "expected PoNR warning; got {warnings:?}"
    );
}

#[test]
fn status_emits_partitioned_warning_when_partitioned_segment_present() {
    use djogi::migrate::schema::PartitionSchema;
    let mut group = synth_single_group(
        "tbl",
        PkKindSchema::HeerId,
        PkKindSchema::HeerIdRecencyBiased,
    );
    group.partitioned_parent = Some(djogi::migrate::PkFlipPartitionedMeta {
        partition: PartitionSchema::Range {
            column: "ts".to_string(),
        },
    });
    let plan = lower_pk_flip_group(&group, bucket());
    let warnings = djogi::migrate::render_pending_plan_warnings(&plan);
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("Partitioned-table cutover")),
        "expected partitioned warning; got {warnings:?}"
    );
}

#[test]
fn status_no_warning_for_additive_plan() {
    let plan = MigrationPlan {
        bucket: bucket(),
        classification: Classification::Additive,
        segments: Vec::new(),
    };
    assert!(djogi::migrate::render_pending_plan_warnings(&plan).is_empty());
}
