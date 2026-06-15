// Internal RLS isolation probe for recursive
// tree queries under a restricted Postgres role.

use djogi::prelude::*;

#[model(
  table = "tree_tenant_node",
  pk = HeerId,
  tree_edge = "parent_id",
  tenant_key = "org_id"
)]
#[derive(Debug, Clone)]
pub struct TenantTreeNode {
  pub org_id: i64,
  pub name: String,
  pub parent_id: Option<ForeignKey<TenantTreeNode>>,
}

async fn setup_tenant_tree_node(ctx: &mut DjogiContext) {
  ctx.raw_execute(
    "ALTER TABLE tree_tenant_node ENABLE ROW LEVEL SECURITY",
    &[],
  )
  .await
  .expect("enable RLS");
  ctx.raw_execute(
    "ALTER TABLE tree_tenant_node FORCE ROW LEVEL SECURITY",
    &[],
  )
  .await
  .expect("force RLS");
  ctx.raw_execute(
    "CREATE POLICY tree_tenant_iso ON tree_tenant_node \
     USING (org_id = current_setting('app.tenant_id', true)::bigint)",
    &[],
  )
  .await
  .expect("create RLS policy");

  let role_exists: bool = ctx
    .raw_scalar(
      "SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'djogi_rls_test_user')",
      &[],
    )
    .await
    .expect("check role");
  if !role_exists {
    ctx.raw_ddl("CREATE ROLE djogi_rls_test_user")
      .await
      .expect("create role");
  }
  ctx.raw_execute(
    "GRANT SELECT, INSERT ON tree_tenant_node TO djogi_rls_test_user",
    &[],
  )
  .await
  .expect("grant table");
  ctx.raw_execute("GRANT USAGE ON SCHEMA public TO djogi_rls_test_user", &[])
    .await
    .expect("grant schema");
  ctx.raw_execute(
    "GRANT EXECUTE ON FUNCTION generate_id() TO djogi_rls_test_user",
    &[],
  )
  .await
  .expect("grant generate_id");
  ctx.raw_execute("GRANT SELECT ON heer_nodes TO djogi_rls_test_user", &[])
    .await
    .expect("grant heer_nodes");
  ctx.raw_execute(
    "GRANT SELECT, INSERT, UPDATE ON heer_node_state TO djogi_rls_test_user",
    &[],
  )
  .await
  .expect("grant heer_node_state");
  ctx.raw_execute("GRANT SELECT ON heer_config TO djogi_rls_test_user", &[])
    .await
    .expect("grant heer_config");
}

#[djogi::djogi_test(sync_models = [TenantTreeNode])]
async fn rls_tenant_isolation_descendants(mut ctx: DjogiContext) {
  setup_tenant_tree_node(&mut ctx).await;
  let pool = ctx
    .share_pool()
    .expect("djogi_test context must be pool-backed for outer atomic scopes");

  let a_root_id: HeerId = djogi::transaction::atomic(&pool, |tx| {
    Box::pin(async move {
      tx.raw_execute("SET LOCAL ROLE djogi_rls_test_user", &[])
        .await?;
      tx.set_tenant("1000").await?;
      let root = TenantTreeNode::create(
        tx,
        TenantTreeNode {
          id: <HeerId as PrimaryKey>::sentinel(),
          created_at: DateTime::UNIX_EPOCH,
          updated_at: DateTime::UNIX_EPOCH,
          org_id: 1000,
          name: "a-root".into(),
          parent_id: None,
        },
      )
      .await?;
      let l1 = TenantTreeNode::create(
        tx,
        TenantTreeNode {
          id: <HeerId as PrimaryKey>::sentinel(),
          created_at: DateTime::UNIX_EPOCH,
          updated_at: DateTime::UNIX_EPOCH,
          org_id: 1000,
          name: "a-l1".into(),
          parent_id: Some(ForeignKey::new(root.id)),
        },
      )
      .await?;
      let _l2 = TenantTreeNode::create(
        tx,
        TenantTreeNode {
          id: <HeerId as PrimaryKey>::sentinel(),
          created_at: DateTime::UNIX_EPOCH,
          updated_at: DateTime::UNIX_EPOCH,
          org_id: 1000,
          name: "a-l2".into(),
          parent_id: Some(ForeignKey::new(l1.id)),
        },
      )
      .await?;
      Ok::<_, DjogiError>(root.id)
    })
  })
  .await
  .expect("tenant A seed");

  djogi::transaction::atomic(&pool, |tx| {
    Box::pin(async move {
      tx.raw_execute("SET LOCAL ROLE djogi_rls_test_user", &[])
        .await?;
      tx.set_tenant("2000").await?;
      let _ = TenantTreeNode::create(
        tx,
        TenantTreeNode {
          id: <HeerId as PrimaryKey>::sentinel(),
          created_at: DateTime::UNIX_EPOCH,
          updated_at: DateTime::UNIX_EPOCH,
          org_id: 2000,
          name: "b-root".into(),
          parent_id: None,
        },
      )
      .await?;
      Ok::<_, DjogiError>(())
    })
  })
  .await
  .expect("tenant B seed");

  let leaked = djogi::transaction::atomic(&pool, |tx| {
    Box::pin(async move {
      tx.raw_execute("SET LOCAL ROLE djogi_rls_test_user", &[])
        .await?;
      tx.set_tenant("2000").await?;
      TenantTreeNode::tree_descendants(a_root_id)
        .expect("tree_edge resolves")
        .fetch_all(tx)
        .await
    })
  })
  .await
  .expect("tenant B walk");

  assert!(
    leaked.is_empty(),
    "tenant B must see zero rows; got {} rows: {:?}",
    leaked.len(),
    leaked.iter().map(|n| &n.name).collect::<Vec<_>>()
  );
}
