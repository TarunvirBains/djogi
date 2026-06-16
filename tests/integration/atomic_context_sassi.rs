// Issue #123 — context-rooted atomic transactions
// must not rebuild the parent context's Sassi registry.
//
// The compatibility `atomic(&pool, ...)` path still creates a fresh top-level
// context because no parent `DjogiContext` exists. The request-context path
// (`atomic(&mut pool_ctx, ...)`) is the no-rebuild path for callers that need
// transaction semantics without changing the context/cache boundary.

use djogi::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[model(table = "atomic_rows", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct AtomicRow {
    pub label: String,
}

#[djogi::djogi_test(sync_models = [AtomicRow])]
async fn pool_backed_context_atomic_reuses_parent_sassi(mut ctx: djogi::DjogiContext) {
    let parent_punnu = ctx
        .punnu::<AtomicRow>()
        .expect("AtomicRow must register a Punnu boot hook");

    let tx_used_parent_punnu = djogi::transaction::atomic(&mut ctx, |tx| {
        let parent_punnu = parent_punnu.clone();
        Box::pin(async move {
            AtomicRow::create(
                tx,
                AtomicRow {
                    label: "committed".to_owned(),
                    ..Default::default()
                },
            )
            .await?;
            let tx_punnu = tx
                .punnu::<AtomicRow>()
                .expect("transaction context must see the registered Punnu");
            Ok::<_, djogi::DjogiError>(std::sync::Arc::ptr_eq(&parent_punnu, &tx_punnu))
        })
    })
    .await
    .expect("pool-backed context atomic should commit");

    assert!(
        tx_used_parent_punnu,
        "atomic(&mut pool_ctx, ...) must share the parent context's Punnu; \
         a rebuilt Sassi would allocate a distinct Punnu"
    );

    let count = AtomicRow::objects()
        .count(&mut ctx)
        .await
        .expect("count committed rows");
    assert_eq!(count, 1, "pool-backed context atomic must commit on Ok");
}

#[djogi::djogi_test(sync_models = [AtomicRow])]
async fn compatibility_pool_atomic_keeps_fresh_context_boundary(mut ctx: djogi::DjogiContext) {
    let parent_punnu = ctx
        .punnu::<AtomicRow>()
        .expect("AtomicRow must register a Punnu boot hook");
    let pool = ctx
        .share_pool()
        .expect("djogi_test harness returns a pool-backed context");

    let pool_atomic_used_parent_punnu = djogi::transaction::atomic(&pool, |tx| {
        let parent_punnu = parent_punnu.clone();
        Box::pin(async move {
            let tx_punnu = tx
                .punnu::<AtomicRow>()
                .expect("transaction context must see the registered Punnu");
            Ok::<_, djogi::DjogiError>(std::sync::Arc::ptr_eq(&parent_punnu, &tx_punnu))
        })
    })
    .await
    .expect("pool atomic compatibility path should commit");

    assert!(
        !pool_atomic_used_parent_punnu,
        "atomic(&pool, ...) has no parent context and must keep a fresh \
         top-level Sassi boundary"
    );
}

#[djogi::djogi_test(sync_models = [AtomicRow])]
async fn pool_backed_context_atomic_rolls_back_on_err(mut ctx: djogi::DjogiContext) {
    ctx.set_tenant("stale-pool-flag")
        .await
        .expect("pool-backed set_tenant should issue its one statement");
    assert!(
        ctx.tenant_set,
        "test precondition: parent pool context has stale tenant tracker state"
    );

    let result = djogi::transaction::atomic(&mut ctx, |tx| {
        Box::pin(async move {
            AtomicRow::create(
                tx,
                AtomicRow {
                    label: "rolled-back".to_owned(),
                    ..Default::default()
                },
            )
            .await?;
            Err::<(), _>(djogi::DjogiError::not_found("forced rollback"))
        })
    })
    .await;

    assert!(result.is_err(), "closure Err must surface");
    assert!(
        !ctx.tenant_set,
        "rollback must clear stale pool-context tenant tracker state"
    );

    let count = AtomicRow::objects()
        .count(&mut ctx)
        .await
        .expect("count rows after rollback");
    assert_eq!(count, 0, "pool-backed context atomic must roll back on Err");
}

#[djogi::djogi_test(sync_models = [AtomicRow])]
async fn pool_backed_context_atomic_carries_auth_on_success(mut ctx: djogi::DjogiContext) {
    ctx.set_auth(
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("valid HeerId"))
            .with_tenant("org_a"),
    );
    ctx.set_no_tenant_scope();

    djogi::transaction::atomic(&mut ctx, |tx| {
        Box::pin(async move {
            assert_eq!(
                tx.auth().and_then(|auth| auth.tenant_id.as_deref()),
                Some("org_a"),
                "pool-backed context atomic must copy parent auth into tx context"
            );
            assert!(
                tx.tenant_scope_suppressed(),
                "pool-backed context atomic must copy tenant-scope suppression into tx context"
            );

            tx.set_auth(
                djogi::auth::AuthContext::new(djogi::HeerId::from_i64(2).expect("valid HeerId"))
                    .with_tenant("org_b"),
            );
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .expect("pool-backed context atomic should commit");

    assert_eq!(
        ctx.auth().and_then(|auth| auth.tenant_id.as_deref()),
        Some("org_b"),
        "successful atomic(&mut pool_ctx, ...) should propagate auth mutations back"
    );
    assert!(
        ctx.tenant_scope_suppressed(),
        "successful atomic(&mut pool_ctx, ...) should preserve tenant-scope suppression"
    );
}

#[djogi::djogi_test(sync_models = [AtomicRow])]
async fn pool_backed_context_atomic_drains_on_commit(mut ctx: djogi::DjogiContext) {
    let fired = Arc::new(AtomicUsize::new(0));

    djogi::transaction::atomic(&mut ctx, |tx| {
        let fired = Arc::clone(&fired);
        Box::pin(async move {
            tx.on_commit(move || async move {
                fired.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await
    .expect("pool-backed context atomic should commit");

    assert_eq!(
        fired.load(Ordering::SeqCst),
        1,
        "on_commit registered on the tx context must drain exactly once after commit"
    );
}

#[djogi::djogi_test(sync_models = [AtomicRow])]
async fn pool_backed_context_atomic_rolls_back_on_panic(mut ctx: djogi::DjogiContext) {
    let result = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
        djogi::transaction::atomic::<_, _, ()>(&mut ctx, |tx| {
            Box::pin(async move {
                AtomicRow::create(
                    tx,
                    AtomicRow {
                        label: "panic-rolled-back".to_owned(),
                        ..Default::default()
                    },
                )
                .await?;
                panic!("forced panic from pool-backed context atomic")
            })
        }),
    ))
    .await;

    assert!(result.is_err(), "closure panic must resume after rollback");

    let count = AtomicRow::objects()
        .count(&mut ctx)
        .await
        .expect("count rows after panic rollback");
    assert_eq!(
        count, 0,
        "pool-backed context atomic must roll back before resuming panic"
    );
}
