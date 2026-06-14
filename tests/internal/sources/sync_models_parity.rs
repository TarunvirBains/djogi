// -01 — assert that the two table-creation paths produce
// byte-identical schema state.
//
// # What this proves
//
// Djogi has two ways to materialise a model set into a live database:
//
// - **Path A — `sync_models`** (): the test-time helper
//   the `#[djogi_test(sync_models = [...])]` macro emits a call to.
//   It projects descriptors, diffs against an empty source, and
//   calls `execute_plan` to run the resulting SQL directly — no
//   ledger, no advisory lock, no classification gate.
//
// - **Path B — `apply_plan`** (): the production migration
//   runner. It takes the same `MigrationPlan` `sync_models` would
//   build, then routes it through `apply_plan_inner` which inserts
//   a pending ledger row, acquires a Postgres advisory lock for
//   the bucket, runs the relpages probe before any `CREATE INDEX`,
//   walks segments transactionally / non-transactionally as
//   classified, persists the snapshot, and marks the ledger row
//   `applied`.
//
// Both paths share the projection → diff → `plan_delta` pipeline
// (now exposed as the public [`build_sync_plans`] helper). They
// diverge at execute. For an additive plan from an empty DB, the
// resulting `pg_class` / `pg_attribute` / `pg_constraint` /
// `pg_index` state must be byte-identical — if the two execution
// wrappers ever drift (segment ordering, DDL escaping, statement
// merging), this test catches it before merge.
//
// # Why two test databases
//
// `#[djogi_test]` provisions one ephemeral DB. This test needs two
// (one per path) so it sets up databases manually via
// [`djogi::testing::setup_test_db`] / [`teardown_test_db`].

use std::collections::BTreeMap;
use std::time::Duration;

use djogi::descriptor::ModelDescriptor;
use djogi::migrate::{
    RunnerCtx, RunnerIdentity, WorkspaceGuard, acquire_workspace_lock, apply_plan,
    compute_checksum,
};
use djogi::prelude::*;
use djogi::relation::ForeignKey;
use djogi::testing::{build_sync_plans, setup_test_db, teardown_test_db};

// ── Test models ────────────────────────────────────────────────────────────
//
// Two FK-related models. The migration engine's topo-sort + the
// runner's classification gate both have to handle this shape; if
// either path emits operations in the wrong order, the FK
// constraint creation fails on Path B and pg_class diverges.

#[model(table = "parity_categories", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct ParityCategory {
    pub name: String,
    pub display_order: i32,
}

#[model(table = "parity_widgets", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct ParityWidget {
    pub category_id: ForeignKey<ParityCategory>,
    pub name: String,
    pub price_cents: i32,
    pub sku: String,
}

// ── Workspace lock helper ─────────────────────────────────────────────────

fn acquire_test_workspace_guard() -> WorkspaceGuard {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!("djogi-parity-{stamp}.lock"));
    acquire_workspace_lock(&path, Duration::from_secs(2)).expect("acquire workspace lock")
}

// ── pg_catalog parity check ───────────────────────────────────────────────

type ColumnRow = (String, String, bool, Option<String>);
type ForeignKeyRow = (String, String, String, String);
type IndexRow = (String, bool, String, Vec<String>);

#[derive(Debug, Clone, PartialEq, Eq)]
struct PgShape {
    tables: Vec<String>,
    columns: BTreeMap<String, Vec<ColumnRow>>,
    primary_keys: BTreeMap<String, Vec<String>>,
    foreign_keys: BTreeMap<String, Vec<ForeignKeyRow>>,
    indexes: BTreeMap<String, Vec<IndexRow>>,
}

async fn read_pg_shape(ctx: &mut DjogiContext) -> PgShape {
    let table_rows = ctx
        .raw_rows(
            "SELECT c.relname::text \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind = 'r' \
               AND n.nspname = 'public' \
               AND c.relname <> 'djogi_schema_migrations' \
               AND c.relname NOT LIKE 'heeranjid\\_%' ESCAPE '\\' \
             ORDER BY c.relname",
            &[],
        )
        .await
        .expect("read tables");
    let tables: Vec<String> = table_rows
        .iter()
        .map(|r| r.try_get::<_, String>(0).unwrap())
        .collect();

    let col_rows = ctx
        .raw_rows(
            "SELECT c.relname::text, a.attname::text, \
                    format_type(a.atttypid, a.atttypmod)::text, \
                    a.attnotnull, pg_get_expr(d.adbin, d.adrelid) \
             FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE n.nspname = 'public' AND c.relkind = 'r' \
               AND a.attnum > 0 AND NOT a.attisdropped \
               AND c.relname <> 'djogi_schema_migrations' \
             ORDER BY c.relname, a.attnum",
            &[],
        )
        .await
        .expect("read columns");
    let mut columns: BTreeMap<String, Vec<ColumnRow>> = BTreeMap::new();
    for row in &col_rows {
        let table: String = row.try_get(0).unwrap();
        let name: String = row.try_get(1).unwrap();
        let sql_type: String = row.try_get(2).unwrap();
        let notnull: bool = row.try_get(3).unwrap();
        let default: Option<String> = row.try_get(4).unwrap();
        columns
            .entry(table)
            .or_default()
            .push((name, sql_type, notnull, default));
    }

    let pk_rows = ctx
        .raw_rows(
            "SELECT c.relname::text, a.attname::text \
             FROM pg_constraint con \
             JOIN pg_class c ON c.oid = con.conrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_attribute a ON a.attrelid = con.conrelid \
                                 AND a.attnum = ANY(con.conkey) \
             WHERE n.nspname = 'public' AND con.contype = 'p' \
               AND c.relname <> 'djogi_schema_migrations' \
             ORDER BY c.relname, array_position(con.conkey, a.attnum)",
            &[],
        )
        .await
        .expect("read PKs");
    let mut primary_keys: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in &pk_rows {
        let table: String = row.try_get(0).unwrap();
        let col: String = row.try_get(1).unwrap();
        primary_keys.entry(table).or_default().push(col);
    }

    let fk_rows = ctx
        .raw_rows(
            "SELECT c.relname::text, sa.attname::text, tc.relname::text, ta.attname::text, \
                    con.confdeltype::text \
             FROM pg_constraint con \
             JOIN pg_class c ON c.oid = con.conrelid \
             JOIN pg_class tc ON tc.oid = con.confrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_attribute sa ON sa.attrelid = con.conrelid \
                                  AND sa.attnum = con.conkey[1] \
             JOIN pg_attribute ta ON ta.attrelid = con.confrelid \
                                  AND ta.attnum = con.confkey[1] \
             WHERE n.nspname = 'public' AND con.contype = 'f' \
               AND c.relname <> 'djogi_schema_migrations' \
             ORDER BY c.relname, sa.attname",
            &[],
        )
        .await
        .expect("read FKs");
    let mut foreign_keys: BTreeMap<String, Vec<ForeignKeyRow>> = BTreeMap::new();
    for row in &fk_rows {
        let src_table: String = row.try_get(0).unwrap();
        let src_col: String = row.try_get(1).unwrap();
        let tgt_table: String = row.try_get(2).unwrap();
        let tgt_col: String = row.try_get(3).unwrap();
        let on_del: String = row.try_get(4).unwrap();
        foreign_keys
            .entry(src_table)
            .or_default()
            .push((src_col, tgt_table, tgt_col, on_del));
    }

    let idx_rows = ctx
        .raw_rows(
            "SELECT t.relname::text, i.relname::text, ix.indisunique, am.amname::text, \
                    a.attname::text \
             FROM pg_index ix \
             JOIN pg_class i ON i.oid = ix.indexrelid \
             JOIN pg_class t ON t.oid = ix.indrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             JOIN pg_am am ON am.oid = i.relam \
             JOIN unnest(ix.indkey) WITH ORDINALITY AS k(attnum, ord) ON TRUE \
             JOIN pg_attribute a ON a.attrelid = ix.indrelid AND a.attnum = k.attnum \
             WHERE n.nspname = 'public' \
               AND t.relname <> 'djogi_schema_migrations' \
               AND a.attnum > 0 \
             ORDER BY t.relname, i.relname, k.ord",
            &[],
        )
        .await
        .expect("read indexes");
    type IndexKey = (String, String, bool, String);
    let mut indexes_raw: BTreeMap<IndexKey, Vec<String>> = BTreeMap::new();
    for row in &idx_rows {
        let table: String = row.try_get(0).unwrap();
        let idx_name: String = row.try_get(1).unwrap();
        let is_unique: bool = row.try_get(2).unwrap();
        let am: String = row.try_get(3).unwrap();
        let col_name: String = row.try_get(4).unwrap();
        indexes_raw
            .entry((table, idx_name, is_unique, am))
            .or_default()
            .push(col_name);
    }
    let mut indexes: BTreeMap<String, Vec<IndexRow>> = BTreeMap::new();
    for ((table, idx_name, is_unique, am), cols) in indexes_raw {
        indexes
            .entry(table)
            .or_default()
            .push((idx_name, is_unique, am, cols));
    }

    PgShape {
        tables,
        columns,
        primary_keys,
        foreign_keys,
        indexes,
    }
}

// ── The parity test ───────────────────────────────────────────────────────

#[tokio::test]
async fn sync_models_and_apply_plan_produce_identical_pg_class() {
    let descriptors: &[&'static ModelDescriptor] = &[
        <ParityCategory as Model>::descriptor(),
        <ParityWidget as Model>::descriptor(),
    ];

    // Two ephemeral databases — one per execution path.
    let (cleanup_a, mut ctx_a) = setup_test_db().await.expect("setup DB A");
    let (cleanup_b, mut ctx_b) = setup_test_db().await.expect("setup DB B");

    // Both paths build plans the same way: build_sync_plans is the
    // public helper sync_models itself uses, so the test feeds the
    // same plan list to both execute wrappers.
    let plans = build_sync_plans(descriptors).expect("build_sync_plans");
    assert!(
        !plans.is_empty(),
        "expected at least one plan for two-model fixture"
    );

    // Path A — sync_models (calls execute_plan internally)
    djogi::testing::sync_models(&mut ctx_a, descriptors)
        .await
        .expect("Path A: sync_models must succeed");

    // Path B — apply_plan via the runner, walking the same plans
    let guard = acquire_test_workspace_guard();
    for (idx, plan) in plans.iter().enumerate() {
        let up_frags: Vec<&str> = plan
            .segments
            .iter()
            .flat_map(|s| s.statements.iter().map(|st| st.up.as_str()))
            .collect();
        let runner_ctx = RunnerCtx {
            bucket: plan.bucket.clone(),
            version: format!("V20260428000000_parity_{idx}"),
            description: "-01 sync_models <-> apply_plan parity test".to_string(),
            checksum_up: compute_checksum(up_frags),
            checksum_down: None,
            // Snapshot persistence intentionally disabled — we compare
            // pg_class shape, not snapshot file contents.
            snapshot: None,
            snapshot_path: None,
            config: djogi::config::MigrateConfig::default(),
            out_of_order_policy: djogi::migrate::OutOfOrderPolicy::default_for_config(
                &djogi::config::DjogiConfig::default(),
            ),
            audit_pool: None,
            drift_baseline: djogi::migrate::DriftBaseline::Disabled,
            runner_identity: Some(RunnerIdentity::SingleNodeDev),
        };
        apply_plan(&mut ctx_b, plan, &runner_ctx, &guard)
            .await
            .expect("Path B: apply_plan must succeed");
    }

    // Compare raw pg_catalog shape on both DBs.
    let shape_a = read_pg_shape(&mut ctx_a).await;
    let shape_b = read_pg_shape(&mut ctx_b).await;
    assert_eq!(
        shape_a, shape_b,
        "Path A (sync_models) and Path B (apply_plan) produced different pg_class shapes — \
         the two execution wrappers have drifted. \
         Path A:\n{shape_a:#?}\nPath B:\n{shape_b:#?}"
    );

    teardown_test_db(cleanup_a).await;
    teardown_test_db(cleanup_b).await;
}
