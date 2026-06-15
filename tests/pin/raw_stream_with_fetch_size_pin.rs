use futures::StreamExt;
use std::future::pending;
use std::time::Duration;

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): exercises raw_stream_with_fetch_size itself
#[djogi::djogi_test]
async fn raw_stream_with_fetch_size_yields_cursor_rows(ctx: djogi::DjogiContext) {
    let pool = ctx
        .raw_pool()
        .expect("djogi_test context is pool-backed")
        .clone();

    let seen = djogi::transaction::atomic(&pool, |inner| {
        Box::pin(async move {
            let mut stream = inner
                .raw_stream_with_fetch_size(
                    "SELECT value FROM generate_series(1, 4) AS value ORDER BY value",
                    &[],
                    2,
                )
                .await?;

            let mut seen = Vec::new();
            while let Some(row) = stream.next().await {
                let row = row?;
                seen.push(row.try_get::<_, i32>("value")?);
            }

            Ok::<_, djogi::DjogiError>(seen)
        })
    })
    .await
    .expect("raw_stream_with_fetch_size should work inside atomic");

    assert_eq!(seen, vec![1, 2, 3, 4]);
}

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): proves raw_stream_with_fetch_size refuses poisoned transactions
#[djogi::djogi_test]
async fn raw_stream_with_fetch_size_refuses_poisoned_transaction(ctx: djogi::DjogiContext) {
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
            panic!("nested raw_stream_with_fetch_size pin future completed before cancellation: {result:?}")
          }
          ready = ready_rx => ready.expect("inner savepoint should signal readiness"),
        }

        let timeout = tokio::time::timeout(Duration::from_millis(25), &mut inner).await;
        assert!(
          timeout.is_err(),
          "timeout must drop the nested atomic future before cleanup"
        );
      }

      let stream_err = outer
        .raw_stream_with_fetch_size("SELECT 1 AS value", &[], 2)
        .await;
      assert!(
        matches!(
          stream_err,
          Err(djogi::DjogiError::TransactionPoisoned {
            reason: "nested atomic future dropped before savepoint cleanup",
            ..
          })
        ),
        "poisoned transaction must refuse raw_stream_with_fetch_size"
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
