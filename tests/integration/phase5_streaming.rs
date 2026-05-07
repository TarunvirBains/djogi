use djogi::prelude::*;
use futures::StreamExt;

#[model(table = "stream_posts")]
#[derive(Debug, Clone, PartialEq)]
pub struct StreamPost {
    pub title: String,
    pub seq: i32,
}

async fn insert_rows(ctx: &mut djogi::DjogiContext, n: i32) {
    for seq in 1..=n {
        StreamPost::create(
            ctx,
            StreamPost {
                title: format!("row {seq}"),
                seq,
                ..Default::default()
            },
        )
        .await
        .expect("create stream row");
    }
}

#[djogi::djogi_test(sync_models = [StreamPost])]
async fn stream_decodes_every_row(mut ctx: djogi::DjogiContext) {
    insert_rows(&mut ctx, 500).await;

    let mut tx = ctx.begin().await.expect("begin stream transaction");
    let mut stream = StreamPost::objects()
        .order_by(|f| f.seq().asc())
        .stream(&mut tx)
        .await
        .expect("stream construction must succeed inside a transaction");

    let mut seen = Vec::new();
    while let Some(result) = stream.next().await {
        seen.push(result.expect("row decode must not fail").seq);
    }
    drop(stream);
    tx.commit().await.expect("commit stream transaction");

    assert_eq!(seen.len(), 500);
    assert_eq!(seen.first(), Some(&1));
    assert_eq!(seen.last(), Some(&500));
}

#[djogi::djogi_test(sync_models = [StreamPost])]
async fn stream_respects_fetch_size(mut ctx: djogi::DjogiContext) {
    insert_rows(&mut ctx, 125).await;

    let mut tx = ctx.begin().await.expect("begin stream transaction");
    let mut stream = StreamPost::objects()
        .order_by(|f| f.seq().asc())
        .stream_with_fetch_size(&mut tx, 25)
        .await
        .expect("stream_with_fetch_size must succeed inside a transaction");

    let mut next_seq = 1;
    while let Some(result) = stream.next().await {
        let post = result.expect("row decode must not fail");
        assert_eq!(post.seq, next_seq);
        next_seq += 1;
    }
    drop(stream);
    tx.commit().await.expect("commit stream transaction");

    assert_eq!(next_seq, 126);
}

#[djogi::djogi_test(sync_models = [StreamPost])]
async fn stream_outside_transaction_returns_error(mut ctx: djogi::DjogiContext) {
    let result = StreamPost::objects().stream(&mut ctx).await;

    assert!(
        matches!(result, Err(DjogiError::StreamOutsideTransaction)),
        "expected StreamOutsideTransaction",
    );
}

#[djogi::djogi_test(sync_models = [StreamPost])]
async fn exhausted_stream_releases_transaction_for_more_typed_work(mut ctx: djogi::DjogiContext) {
    insert_rows(&mut ctx, 20).await;

    let mut tx = ctx.begin().await.expect("begin stream transaction");
    let mut stream = StreamPost::objects()
        .stream_with_fetch_size(&mut tx, 5)
        .await
        .expect("stream construction");

    let mut count = 0;
    while let Some(result) = stream.next().await {
        result.expect("row decode must not fail");
        count += 1;
    }
    drop(stream);

    let after_stream = StreamPost::objects()
        .fetch_all(&mut tx)
        .await
        .expect("typed query after stream exhaustion");
    tx.commit().await.expect("commit stream transaction");

    assert_eq!(count, 20);
    assert_eq!(after_stream.len(), 20);
}

#[djogi::djogi_test(sync_models = [StreamPost])]
async fn transaction_rollback_discards_typed_writes_after_stream(mut ctx: djogi::DjogiContext) {
    insert_rows(&mut ctx, 5).await;

    let mut tx = ctx.begin().await.expect("begin stream transaction");
    StreamPost::create(
        &mut tx,
        StreamPost {
            title: "rolled back".into(),
            seq: 999,
            ..Default::default()
        },
    )
    .await
    .expect("create row inside transaction");

    let mut stream = StreamPost::objects()
        .filter(|f| f.seq().gte(999))
        .stream(&mut tx)
        .await
        .expect("stream sees transaction-local row");
    assert!(stream.next().await.expect("one row").is_ok());
    drop(stream);

    tx.rollback().await.expect("rollback transaction");

    let after_rollback = StreamPost::objects()
        .filter(|f| f.seq().gte(999))
        .fetch_all(&mut ctx)
        .await
        .expect("typed query after rollback");
    assert!(after_rollback.is_empty());
}
