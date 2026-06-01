// Cluster 3 issue #173 — typed
// `DjogiContext::clone_for_concurrent_reads` helper.
//
// # Scope
//
// Cover the four live-PG behaviours of the typed concurrent-reads
// helper:
//
// 1. **`tokio::try_join!` over two clones runs concurrently** —
//    independent pool checkouts, no `E0499` borrow conflict, both
//    branches see the expected rows.
//
// 2. **The clone preserves the parent's `Sassi` cache registry** —
//    `Arc::ptr_eq` on `Punnu<T>` returns `true` because both
//    contexts share the same `Arc<Sassi>` (per the cluster 8δ
//    "DjogiContext IS the cache boundary" contract).
//
// 3. **The clone preserves auth state**, so RLS continues to apply.
//
// 4. **Transaction-backed contexts are rejected** with
//    `DjogiError::ConcurrentReadsRequirePoolContext` — a typed
//    terminal error surfacing the structural constraint.
//
// # Spec / memory anchors
//
// - djogi#173 issue body (closing-condition checklist).
// - `docs/guide/transactions.md` §"Concurrent reads —
//   `clone_for_concurrent_reads`".
// - `feedback_djogi_local_postgres.md` — `#[djogi_test]` provisions a
//   fresh DB per test.

use djogi::prelude::*;
use djogi::transaction::atomic;

#[model(table = "djogi_concurrent_rows", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct ConcurrentRow {
    pub kind: String,
    pub seq: i32,
}

#[djogi::djogi_test(sync_models = [ConcurrentRow])]
async fn concurrent_reads_via_try_join_succeed(mut ctx: djogi::DjogiContext) {
    // Seed two kinds. Sequential creates on the parent context.
    ConcurrentRow::create(
        &mut ctx,
        ConcurrentRow {
            kind: "alpha".to_string(),
            seq: 1,
            ..Default::default()
        },
    )
    .await
    .expect("alpha seed");
    ConcurrentRow::create(
        &mut ctx,
        ConcurrentRow {
            kind: "beta".to_string(),
            seq: 2,
            ..Default::default()
        },
    )
    .await
    .expect("beta seed");

    // Clone twice — each clone gets its own pool checkout per
    // operation, so `try_join!` runs the two reads concurrently
    // without aliasing one connection across futures.
    let mut ctx_a = ctx
        .clone_for_concurrent_reads()
        .expect("clone_for_concurrent_reads on pool-backed context must succeed");
    let mut ctx_b = ctx
        .clone_for_concurrent_reads()
        .expect("clone_for_concurrent_reads on pool-backed context must succeed");

    let (alpha, beta) = tokio::try_join!(
        ConcurrentRow::objects()
            .filter(|f| f.kind().eq("alpha".to_string()))
            .fetch_all(&mut ctx_a),
        ConcurrentRow::objects()
            .filter(|f| f.kind().eq("beta".to_string()))
            .fetch_all(&mut ctx_b),
    )
    .expect("concurrent try_join! over two clones");

    assert_eq!(alpha.len(), 1, "alpha branch saw exactly one row");
    assert_eq!(beta.len(), 1, "beta branch saw exactly one row");
    assert_eq!(alpha[0].kind, "alpha");
    assert_eq!(beta[0].kind, "beta");
}

#[djogi::djogi_test(sync_models = [ConcurrentRow])]
async fn clone_for_concurrent_reads_shares_sassi_registry(mut ctx: djogi::DjogiContext) {
    // The clone must share the parent's `Arc<Sassi>` so cache
    // writes through one clone are visible to reads through
    // the other (the "DjogiContext IS the cache boundary"
    // contract from cluster 8δ T7.4).
    let parent_punnu = ctx
        .punnu::<ConcurrentRow>()
        .expect("ConcurrentRow must register a Punnu boot hook");

    let cloned = ctx
        .clone_for_concurrent_reads()
        .expect("clone_for_concurrent_reads on pool-backed context");
    let cloned_punnu = cloned
        .punnu::<ConcurrentRow>()
        .expect("clone must observe the same Punnu registry");

    assert!(
        std::sync::Arc::ptr_eq(&parent_punnu, &cloned_punnu),
        "clone_for_concurrent_reads must share the parent's Arc<Sassi> \
         so cache state is consistent across the clones",
    );
}

#[djogi::djogi_test(sync_models = [ConcurrentRow])]
async fn clone_for_concurrent_reads_preserves_auth(mut ctx: djogi::DjogiContext) {
    // The clone must copy auth state so RLS continues to apply to
    // concurrent reads. Set auth on the parent, clone, and assert
    // the clone observes the same tenant id.
    ctx.set_auth(
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(42).expect("valid HeerId"))
            .with_tenant("org_a"),
    );
    ctx.set_no_tenant_scope();

    let cloned = ctx
        .clone_for_concurrent_reads()
        .expect("clone_for_concurrent_reads on pool-backed context");

    assert_eq!(
        cloned.auth().and_then(|auth| auth.tenant_id.as_deref()),
        Some("org_a"),
        "clone_for_concurrent_reads must copy auth into the new context",
    );
    assert!(
        cloned.__tenant_scope_suppressed_for_macros(),
        "clone_for_concurrent_reads must preserve tenant-scope suppression",
    );
}

#[djogi::djogi_test(sync_models = [ConcurrentRow])]
async fn clone_for_concurrent_reads_rejects_transaction_context(mut ctx: djogi::DjogiContext) {
    // Inside an open `atomic()` the context is transaction-backed.
    // `clone_for_concurrent_reads` must reject with
    // `ConcurrentReadsRequirePoolContext` — the typed terminal
    // error surfaces the structural constraint synchronously
    // without aliasing the transaction's single connection.
    let result = atomic(&mut ctx, |tx| {
        Box::pin(async move {
            let clone_attempt = tx.clone_for_concurrent_reads();
            match clone_attempt {
                Err(DjogiError::ConcurrentReadsRequirePoolContext) => Ok::<_, DjogiError>(()),
                Err(other) => panic!("expected ConcurrentReadsRequirePoolContext, got {other:?}"),
                Ok(_) => panic!(
                    "expected ConcurrentReadsRequirePoolContext, but clone succeeded on a \
                     transaction-backed context",
                ),
            }
        })
    })
    .await;
    assert!(
        result.is_ok(),
        "outer atomic must complete after typed rejection",
    );
}
