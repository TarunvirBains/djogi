#![allow(clippy::disallowed_methods)]

use std::future::pending;
use std::time::Duration;

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): exercises raw_pool, raw_conn, and raw_with_client themselves
#[djogi::djogi_test]
async fn raw_pool_access_reaches_pool_connection_and_client(ctx: djogi::DjogiContext) {
    assert!(
        ctx.raw_pool().is_some(),
        "djogi_test context is pool-backed"
    );
    assert!(
        ctx.raw_conn().is_none(),
        "pool-backed context has no transaction connection"
    );

    let pool = ctx
        .raw_pool()
        .expect("djogi_test context is pool-backed")
        .clone();
    let via_pool = pool
        .raw_with_client(|client| {
            Box::pin(async move {
                let row = client.query_one("SELECT 47::integer AS value", &[]).await?;
                Ok(row.try_get::<_, i32>("value")?)
            })
        })
        .await
        .expect("raw_with_client should expose a pooled client");

    let raw_conn_was_present = djogi::transaction::atomic(&pool, |inner| {
        Box::pin(async move { Ok::<_, djogi::DjogiError>(inner.raw_conn().is_some()) })
    })
    .await
    .expect("atomic should create a transaction-backed context");

    assert_eq!(via_pool, 47);
    assert!(raw_conn_was_present);
}

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): proves raw_conn is withheld after nested cancellation poisons the transaction
#[djogi::djogi_test]
async fn raw_conn_is_withheld_after_nested_cancellation_poison(ctx: djogi::DjogiContext) {
    let pool = ctx
        .raw_pool()
        .expect("djogi_test context is pool-backed")
        .clone();

    let outer_result = djogi::transaction::atomic(&pool, |outer| {
        Box::pin(async move {
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
            {
                let inner = djogi::transaction::atomic(&mut *outer, |inner| {
                    Box::pin(async move {
                        inner.raw_execute("SELECT 1", &[]).await?;
                        let _ = ready_tx.send(());
                        pending::<()>().await;
                        #[allow(unreachable_code)]
                        Ok::<_, djogi::DjogiError>(())
                    })
                });
                tokio::pin!(inner);

                tokio::select! {
                    result = &mut inner => {
                        panic!("nested raw_conn pin future completed before cancellation: {result:?}")
                    }
                    ready = ready_rx => ready.expect("inner savepoint should signal readiness"),
                }

                let timeout = tokio::time::timeout(Duration::from_millis(25), &mut inner).await;
                assert!(
                    timeout.is_err(),
                    "timeout must drop the nested atomic future before cleanup"
                );
            }

            assert!(
                outer.raw_conn().is_none(),
                "poisoned transaction must not expose raw_conn"
            );
            Ok::<_, djogi::DjogiError>(())
        })
    })
    .await;

    assert!(
        matches!(
            outer_result,
            Err(djogi::DjogiError::TransactionPoisoned {
                reason: "nested atomic future dropped before savepoint cleanup",
                ..
            })
        ),
        "poisoned outer transaction must fail closed, got: {outer_result:?}"
    );
}
