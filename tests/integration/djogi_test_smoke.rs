//! Smoke test for the `#[djogi_test]` proc-macro lifecycle.
//!
//! This is the ONLY adopter of `#[djogi_test]` through Phase 5-Zero T9.
//! All other integration tests continue to use `#[sqlx::test]` per
//! Phase 5-Zero plan RQ-10. T10 migrates every `#[sqlx::test]` call
//! to `#[djogi_test]` and removes the sqlx dev-dependency.
//!
//! # What this proves
//!
//! The smoke test exercises the full lifecycle end-to-end:
//! 1. A per-test Postgres database is created (`djogi_test_<uuid>`).
//! 2. HeeRanjID schema is installed and the default node seeded.
//! 3. A `DjogiContext` is constructed and passed to the test body.
//! 4. The context is usable for queries (verified with `SELECT 1::bigint`).
//! 5. The database is dropped after the test body returns.

#[djogi::djogi_test]
async fn djogi_test_context_is_usable(ctx: djogi::DjogiContext) {
    // Verify that the context has a valid pool connection by running
    // a trivial query through the underlying pool.
    //
    // ctx.pool() returns Some(&PgPool) for a pool-backed context, which
    // is what setup_test_db produces.
    let pool = ctx
        .pool()
        .expect("djogi_test: context should be pool-backed");

    let result: i64 = sqlx::query_scalar("SELECT 1::bigint")
        .fetch_one(pool)
        .await
        .expect("SELECT 1::bigint should succeed");

    assert_eq!(result, 1, "SELECT 1 should return 1");
}

#[djogi::djogi_test]
async fn djogi_test_heeranjid_is_installed(ctx: djogi::DjogiContext) {
    // Verify that HeeRanjID functions are available — generate_id() returns
    // a positive integer, confirming the extension + node setup ran.
    let pool = ctx
        .pool()
        .expect("djogi_test: context should be pool-backed");

    let id: i64 = sqlx::query_scalar("SELECT generate_id()")
        .fetch_one(pool)
        .await
        .expect("generate_id() should be available after HeeRanjID setup");

    assert!(
        id > 0,
        "generate_id() should return a positive HeerId; got {id}"
    );
}
