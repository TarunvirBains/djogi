//! Phase 5.5 integration tests — auth substrate.
//!
//! Task 1 scope (this file, initially): `DjogiContext::with_auth` attaches
//! an `AuthContext` that can be read back via `ctx.auth()`.
//!
//! Later Phase 5.5 tasks extend this file (Task 4 password_hash_round_trips,
//! Task 10 auto_set_tenant_from_auth, Task 11 with_auth_insecurely_emits_warn).

use djogi::auth::AuthContext;
use djogi::prelude::*;
use djogi_macros::model;

#[djogi::djogi_test]
async fn with_auth_attaches_and_reads_back(mut ctx: djogi::DjogiContext) {
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
#[djogi::djogi_test]
async fn password_hash_round_trips_through_pg(mut ctx: djogi::DjogiContext) {
    ctx.raw_ddl(
        "CREATE TABLE IF NOT EXISTS users_password_hash_roundtrip (
            id BIGINT PRIMARY KEY DEFAULT generate_id(),
            password_hash TEXT NOT NULL
        )",
    )
    .await
    .expect("create table");

    let h = djogi::auth::PasswordHash::hash("s3cret").unwrap();
    ctx.raw_execute(
        "INSERT INTO users_password_hash_roundtrip (password_hash) VALUES ($1)",
        &[&h],
    )
    .await
    .expect("insert");

    let stored: djogi::auth::PasswordHash = ctx
        .raw_scalar(
            "SELECT password_hash FROM users_password_hash_roundtrip LIMIT 1",
            &[],
        )
        .await
        .expect("select");
    assert!(stored.verify("s3cret"));
    assert!(!stored.verify("wrong"));
}

// ── Task 10: auto-set_tenant from auth ────────────────────────────────────

/// Tenant-keyed model for Task 10 tests.
///
/// Declares `org_id` as its tenant column so that `set_tenant("org_id")`
/// drives the `app.tenant_id` GUC that the RLS policy checks.
#[model(table = "phase5_5_tenant_posts", tenant_key = "org_id")]
#[derive(Debug, Clone)]
pub struct TenantPost {
    pub org_id: String,
    pub title: String,
}

async fn setup_tenant_posts(ctx: &mut djogi::DjogiContext) {
    ctx.raw_ddl(
        "CREATE TABLE IF NOT EXISTS phase5_5_tenant_posts (
            id BIGINT PRIMARY KEY DEFAULT generate_id(),
            org_id TEXT NOT NULL,
            title TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
    )
    .await
    .expect("create phase5_5_tenant_posts table");

    ctx.raw_ddl("ALTER TABLE phase5_5_tenant_posts ENABLE ROW LEVEL SECURITY")
        .await
        .expect("enable RLS on phase5_5_tenant_posts");

    // Drop any previously created policy so the setup is idempotent across
    // test runs (Postgres has no `CREATE POLICY IF NOT EXISTS`).
    let _ = ctx
        .raw_ddl("DROP POLICY IF EXISTS phase5_5_tenant_posts_isolation ON phase5_5_tenant_posts")
        .await;

    ctx.raw_ddl(
        "CREATE POLICY phase5_5_tenant_posts_isolation
           ON phase5_5_tenant_posts
           USING (org_id = current_setting('app.tenant_id', true))",
    )
    .await
    .expect("create RLS policy on phase5_5_tenant_posts");
}

/// When `ctx.auth()` carries a `tenant_id`, the first CRUD/QuerySet
/// operation on a tenant-keyed model automatically calls
/// `ctx.ensure_tenant_set(tenant_id)` — the caller does not need to call
/// `ctx.set_tenant(...)` explicitly.
///
/// This test does NOT call `set_tenant` anywhere. It attaches an
/// `AuthContext` with `tenant_id = "org_a"` via `ctx.set_auth(...)` and
/// then calls `TenantPost::objects().fetch_all(ctx)`. The auto-wiring in
/// Task 10 should issue `ensure_tenant_set("org_a")` transparently before
/// the SQL executes, and `ctx.tenant_set` should be `true` afterwards.
#[djogi::djogi_test]
async fn auto_set_tenant_from_auth(mut ctx: djogi::DjogiContext) {
    setup_tenant_posts(&mut ctx).await;

    let pool = ctx.pool().expect("test ctx must be pool-backed").clone();
    djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            let auth = AuthContext::new(HeerId::from_i64(1).unwrap()).with_tenant("org_a");
            tx.set_auth(auth);
            // No explicit set_tenant call — auto-wiring must issue it.
            let posts = TenantPost::objects().fetch_all(tx).await?;
            assert_eq!(posts.len(), 0);
            // Auto-wiring must have set tenant_set = true.
            assert!(tx.tenant_set, "tenant_set must be true after auto-wiring");
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .unwrap();
}

/// When `ctx.auth()` is set but carries no `tenant_id`, the auto-wiring
/// must be a no-op. `ctx.tenant_set` stays `false` and no error is raised.
///
/// The RLS policy uses `current_setting('app.tenant_id', true)` with
/// `missing_ok = true` so an unset GUC returns NULL — the `=` predicate
/// treats NULL as unmatched, so the query succeeds but returns zero rows.
#[djogi::djogi_test]
async fn auto_set_tenant_no_op_when_tenant_id_none(mut ctx: djogi::DjogiContext) {
    setup_tenant_posts(&mut ctx).await;

    let pool = ctx.pool().expect("test ctx must be pool-backed").clone();
    djogi::transaction::atomic(&pool, |tx| {
        Box::pin(async move {
            // Auth present, but no tenant_id — auto-wiring must be a no-op.
            let auth = AuthContext::new(HeerId::from_i64(1).unwrap());
            tx.set_auth(auth);
            // Auto-wiring should not call set_tenant — no tenant_id to apply.
            let posts = TenantPost::objects().fetch_all(tx).await?;
            assert_eq!(posts.len(), 0);
            // tenant_set must remain false — nothing was auto-applied.
            assert!(
                !tx.tenant_set,
                "tenant_set must stay false when auth has no tenant_id"
            );
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .unwrap();
}
