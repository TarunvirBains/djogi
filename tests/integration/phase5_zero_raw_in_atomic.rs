//! Phase 5-Zero T5 — raw SQL methods respect the active `atomic()`
//! transaction / savepoint context.
//!
//! This test proves the new `DjogiContext::{raw_execute, raw_scalar}`
//! surface threads through the existing transaction dispatcher rather
//! than escaping to a separate pool connection.

use djogi::prelude::*;

#[model(table = "t5_raw_atomic_posts")]
#[derive(Debug, Clone)]
pub struct RawAtomicPost {
    pub title: String,
}

async fn setup_tables(ctx: &mut djogi::DjogiContext) {
    ctx.__execute_for_macros(
        "CREATE TABLE IF NOT EXISTS t5_raw_atomic_posts (
            id         BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at TIMESTAMPTZ NOT NULL    DEFAULT now(),
            title      TEXT        NOT NULL
        )",
        &[],
    )
    .await
    .expect("create t5_raw_atomic_posts");
}

#[djogi::djogi_test]
async fn raw_methods_respect_atomic_transaction_scope(mut ctx: djogi::DjogiContext) {
    setup_tables(&mut ctx).await;

    let pool = ctx.pool().unwrap().clone();

    let committed = atomic(&pool, |ctx| {
        Box::pin(async move {
            let before: i64 = ctx
                .raw_scalar("SELECT COUNT(*) FROM t5_raw_atomic_posts", &[])
                .await
                .expect("count before committed insert");

            let title = "committed row".to_string();
            let inserted = ctx
                .raw_execute(
                    "INSERT INTO t5_raw_atomic_posts (title) VALUES ($1)",
                    &[&title],
                )
                .await
                .expect("raw insert inside atomic should succeed");
            assert_eq!(inserted, 1, "single-row insert must affect one row");

            let visible_inside: i64 = ctx
                .raw_scalar("SELECT COUNT(*) FROM t5_raw_atomic_posts", &[])
                .await
                .expect("count after committed insert");
            assert_eq!(
                visible_inside,
                before + 1,
                "raw read inside atomic must see uncommitted write on the same transaction"
            );

            Ok::<i64, DjogiError>(visible_inside)
        })
    })
    .await
    .expect("commit path should succeed");

    let after_commit: i64 = ctx
        .raw_scalar("SELECT COUNT(*) FROM t5_raw_atomic_posts", &[])
        .await
        .expect("count after commit");
    assert_eq!(
        after_commit, committed,
        "committed raw insert must be visible outside atomic after commit"
    );

    let rollback_result = atomic(&pool, |ctx| {
        Box::pin(async move {
            let title = "rolled back row".to_string();
            ctx.raw_execute(
                "INSERT INTO t5_raw_atomic_posts (title) VALUES ($1)",
                &[&title],
            )
            .await
            .expect("raw insert inside rollback path should succeed");

            let visible_inside: i64 = ctx
                .raw_scalar("SELECT COUNT(*) FROM t5_raw_atomic_posts", &[])
                .await
                .expect("count after rollback-path insert");
            assert_eq!(
                visible_inside,
                after_commit + 1,
                "raw read inside rollback path must still see the uncommitted write"
            );

            Err::<(), _>(DjogiError::not_found("force rollback"))
        })
    })
    .await;
    assert!(
        matches!(rollback_result, Err(DjogiError::NotFound { .. })),
        "rollback path must propagate the forced error, got: {:?}",
        rollback_result
    );

    let after_rollback: i64 = ctx
        .raw_scalar("SELECT COUNT(*) FROM t5_raw_atomic_posts", &[])
        .await
        .expect("count after rollback");
    assert_eq!(
        after_rollback, after_commit,
        "rolled-back raw insert must not be visible outside atomic"
    );
}
