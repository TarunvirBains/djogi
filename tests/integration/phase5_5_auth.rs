use djogi::auth::AuthContext;
use djogi::prelude::*;

#[cfg(feature = "auth-argon2")]
#[model(table = "users_password_hash_roundtrip")]
#[derive(Debug, Clone)]
pub struct PasswordUser {
    pub password_hash: djogi::auth::PasswordHash,
}

#[model(table = "phase5_5_tenant_posts", pk = HeerId, tenant_key = "org_id")]
#[derive(Debug, Clone)]
pub struct TenantPost {
    pub org_id: String,
    pub title: String,
}

#[djogi::djogi_test]
async fn with_auth_attaches_and_reads_back(ctx: djogi::DjogiContext) {
    let auth = AuthContext::new(HeerId::from_i64(42).unwrap())
        .with_tenant("org_a")
        .with_scopes(vec!["read".into(), "write".into()]);
    let ctx = ctx.with_auth(auth.clone());
    let attached = ctx.auth().expect("auth attached");

    assert_eq!(attached.user_id, auth.user_id);
    assert_eq!(attached.tenant_id, Some("org_a".into()));
    assert!(attached.has_scope("read"));
}

#[cfg(feature = "auth-argon2")]
#[djogi::djogi_test(sync_models = [PasswordUser])]
async fn password_hash_round_trips_through_model_crud(mut ctx: djogi::DjogiContext) {
    let hash = djogi::auth::PasswordHash::hash("s3cret").unwrap();
    let created = PasswordUser::create(
        &mut ctx,
        PasswordUser {
            password_hash: hash,
            ..Default::default()
        },
    )
    .await
    .expect("create password user");

    let reloaded = PasswordUser::get(&mut ctx, created.id)
        .await
        .expect("reload password user");
    assert!(reloaded.password_hash.verify("s3cret"));
    assert!(!reloaded.password_hash.verify("wrong"));
}

#[djogi::djogi_test(sync_models = [TenantPost])]
async fn auto_set_tenant_from_auth_on_fetch(mut ctx: djogi::DjogiContext) {
    let mut tx = ctx.begin().await.expect("begin transaction");
    tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()).with_tenant("org_a"));

    let posts = TenantPost::objects()
        .fetch_all(&mut tx)
        .await
        .expect("tenant-keyed fetch");

    assert!(posts.is_empty());
    assert_eq!(tx.applied_tenant_id(), Some("org_a"));
    assert!(tx.tenant_set);
    tx.commit().await.expect("commit transaction");
}

#[djogi::djogi_test(sync_models = [TenantPost])]
async fn auto_set_tenant_no_op_when_tenant_id_none(mut ctx: djogi::DjogiContext) {
    let mut tx = ctx.begin().await.expect("begin transaction");
    tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()));
    tx.set_no_tenant_scope();

    let posts = TenantPost::objects()
        .fetch_all(&mut tx)
        .await
        .expect("tenant-keyed fetch");

    assert!(posts.is_empty());
    assert_eq!(tx.applied_tenant_id(), None);
    assert!(!tx.tenant_set);
    tx.commit().await.expect("commit transaction");
}

#[djogi::djogi_test(sync_models = [TenantPost])]
async fn ensure_tenant_set_switches_when_auth_tenant_changes(mut ctx: djogi::DjogiContext) {
    let mut tx = ctx.begin().await.expect("begin transaction");

    tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()).with_tenant("org_a"));
    TenantPost::objects()
        .fetch_all(&mut tx)
        .await
        .expect("first tenant fetch");
    assert_eq!(tx.applied_tenant_id(), Some("org_a"));

    tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()).with_tenant("org_b"));
    TenantPost::objects()
        .fetch_all(&mut tx)
        .await
        .expect("second tenant fetch");
    assert_eq!(tx.applied_tenant_id(), Some("org_b"));

    tx.commit().await.expect("commit transaction");
}

#[djogi::djogi_test(sync_models = [TenantPost])]
async fn auto_set_tenant_clears_applied_tid_when_auth_loses_tenant(mut ctx: djogi::DjogiContext) {
    let mut tx = ctx.begin().await.expect("begin transaction");

    tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()).with_tenant("org_a"));
    TenantPost::objects()
        .fetch_all(&mut tx)
        .await
        .expect("tenant-bound fetch");
    assert_eq!(tx.applied_tenant_id(), Some("org_a"));

    tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()));
    tx.set_no_tenant_scope();
    TenantPost::objects()
        .fetch_all(&mut tx)
        .await
        .expect("tenant-clearing fetch");

    assert_eq!(tx.applied_tenant_id(), None);
    assert!(!tx.tenant_set);
    tx.commit().await.expect("commit transaction");
}

#[djogi::djogi_test(sync_models = [TenantPost])]
async fn bulk_create_auto_sets_tenant_from_auth(mut ctx: djogi::DjogiContext) {
    let mut tx = ctx.begin().await.expect("begin transaction");
    tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()).with_tenant("org_a"));

    let created = TenantPost::bulk_create(
        &mut tx,
        vec![TenantPost {
            org_id: "org_a".to_string(),
            title: "bulk-one".to_string(),
            ..Default::default()
        }],
    )
    .await
    .expect("bulk create tenant post");

    assert_eq!(created.len(), 1);
    assert_eq!(tx.applied_tenant_id(), Some("org_a"));
    tx.commit().await.expect("commit transaction");
}

#[djogi::djogi_test(sync_models = [TenantPost])]
async fn create_with_id_auto_sets_tenant_from_auth(mut ctx: djogi::DjogiContext) {
    let mut tx = ctx.begin().await.expect("begin transaction");
    tx.set_auth(AuthContext::new(HeerId::from_i64(1).unwrap()).with_tenant("org_a"));

    let id = HeerId::from_i64(9_000_000).unwrap();
    let created = TenantPost::create_with_id(
        &mut tx,
        id,
        TenantPost {
            org_id: "org_a".to_string(),
            title: "with-id".to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("create_with_id tenant post");

    assert_eq!(created.id, id);
    assert_eq!(tx.applied_tenant_id(), Some("org_a"));
    tx.commit().await.expect("commit transaction");
}

#[djogi::djogi_test(sync_models = [TenantPost])]
async fn nested_transaction_rollback_restores_auth_state(mut ctx: djogi::DjogiContext) {
    let mut outer = ctx.begin().await.expect("begin outer transaction");
    outer.set_tenant("org_a").await.expect("set outer tenant");
    outer.set_auth(AuthContext::new(HeerId::from_i64(42).unwrap()).with_tenant("org_a"));

    let nested_res: Result<(), djogi::DjogiError> =
        djogi::transaction::atomic(&mut outer, |inner| {
            Box::pin(async move {
                inner.set_tenant("org_b").await?;
                inner
                    .set_auth(AuthContext::new(HeerId::from_i64(99).unwrap()).with_tenant("org_b"));
                Err(djogi::DjogiError::Validation(
                    "intentional rollback".to_string(),
                ))
            })
        })
        .await;

    assert!(nested_res.is_err());
    assert_eq!(outer.applied_tenant_id(), Some("org_a"));
    assert!(outer.tenant_set);
    assert_eq!(
        outer.auth().map(|auth| auth.user_id),
        Some(HeerId::from_i64(42).unwrap()),
    );
    assert_eq!(
        outer.auth().and_then(|auth| auth.tenant_id.clone()),
        Some("org_a".to_string()),
    );
    outer.rollback().await.expect("rollback outer transaction");
}
