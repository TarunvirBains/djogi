use futures::StreamExt;

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
