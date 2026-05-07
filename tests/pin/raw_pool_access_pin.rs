#![allow(clippy::disallowed_methods)]

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
