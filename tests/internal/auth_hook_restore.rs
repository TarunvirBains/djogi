use djogi::auth::AuthContext;
use djogi::__bypass::RawAccessExt;
use djogi::prelude::*;

#[model(
    table = "tenant_hook_posts",
    pk = HeerId,
    tenant_key = "org_id",
    hooks
)]
#[derive(Debug, Clone)]
pub struct TenantHookPost {
    pub org_id: String,
    pub title: String,
}

impl djogi::hooks::ModelHooks for TenantHookPost {
    async fn before_save(
        &mut self,
        _ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        Err(djogi::DjogiError::Validation(
            "before_save abort".to_string(),
        ))
    }

    async fn before_delete(
        &mut self,
        _ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        Err(djogi::DjogiError::Validation(
            "before_delete abort".to_string(),
        ))
    }
}

async fn current_tenant_guc(ctx: &mut djogi::DjogiContext) -> Option<String> {
    let rows = ctx
        .raw_rows("SELECT current_setting('app.tenant_id', true)", &[])
        .await
        .expect("query current tenant guc");
    rows.into_iter()
        .next()
        .expect("single row")
        .try_get::<usize, Option<String>>(0)
        .expect("decode current tenant guc")
}

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION: probes session GUC state to verify tenant scope restoration
#[djogi::djogi_test(sync_models = [TenantHookPost])]
async fn update_returning_pair_hook_err_restores_pre_hook_tenant_scope(
    mut ctx: djogi::DjogiContext,
) {
    let mut tx = ctx.begin().await.expect("begin transaction");

    tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()).with_tenant("org_a"));
    let created = TenantHookPost::create(
        &mut tx,
        TenantHookPost {
            org_id: "org_a".to_string(),
            title: "original".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create tenant hook row");

    assert_eq!(tx.applied_tenant_id(), Some("org_a"));
    assert_eq!(current_tenant_guc(&mut tx).await.as_deref(), Some("org_a"));

    tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()).with_tenant("org_b"));

    let stale = TenantHookPost {
        id: created.id,
        org_id: "org_a".to_string(),
        title: "patched".to_string(),
        ..Default::default()
    };
    let res = stale.update_returning_pair(&mut tx).await;

    let Err(djogi::DjogiError::Validation(msg)) = res else {
        panic!("expected Err(DjogiError::Validation(_)), got {res:?}");
    };
    assert_eq!(msg, "before_save abort");
    assert_eq!(tx.applied_tenant_id(), Some("org_a"));
    assert_eq!(current_tenant_guc(&mut tx).await.as_deref(), Some("org_a"));

    tx.rollback().await.expect("rollback transaction");
}

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION: probes session GUC state to verify tenant scope restoration
#[djogi::djogi_test(sync_models = [TenantHookPost])]
async fn delete_returning_hook_err_restores_pre_hook_tenant_scope(mut ctx: djogi::DjogiContext) {
    let mut tx = ctx.begin().await.expect("begin transaction");

    tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()).with_tenant("org_a"));
    let created = TenantHookPost::create(
        &mut tx,
        TenantHookPost {
            org_id: "org_a".to_string(),
            title: "delete-me".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create tenant hook row");

    assert_eq!(tx.applied_tenant_id(), Some("org_a"));
    assert_eq!(current_tenant_guc(&mut tx).await.as_deref(), Some("org_a"));

    tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()).with_tenant("org_b"));

    let stale = TenantHookPost {
        id: created.id,
        org_id: "org_a".to_string(),
        title: "delete-me".to_string(),
        ..Default::default()
    };
    let res = stale.delete_returning(&mut tx).await;

    let Err(djogi::DjogiError::Validation(msg)) = res else {
        panic!("expected Err(DjogiError::Validation(_)), got {res:?}");
    };
    assert_eq!(msg, "before_delete abort");
    assert_eq!(tx.applied_tenant_id(), Some("org_a"));
    assert_eq!(current_tenant_guc(&mut tx).await.as_deref(), Some("org_a"));

    tx.rollback().await.expect("rollback transaction");
}
