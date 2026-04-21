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
async fn djogi_test_context_is_usable(mut ctx: djogi::DjogiContext) {
    // Verify that the context has a valid pool connection by running
    // a trivial query through the underlying pool's public helper.
    let rows = ctx
        .__query_one_for_macros("SELECT 1::bigint AS val", &[])
        .await
        .expect("SELECT 1::bigint should succeed");

    let result: i64 = rows
        .try_get::<_, i64>("val")
        .expect("val column should decode as i64");

    assert_eq!(result, 1, "SELECT 1 should return 1");
}

#[djogi::djogi_test]
async fn djogi_test_heeranjid_is_installed(mut ctx: djogi::DjogiContext) {
    // Verify that HeeRanjID functions are available — generate_id() returns
    // a positive integer, confirming the extension + node setup ran.
    let row = ctx
        .__query_one_for_macros("SELECT generate_id() AS id", &[])
        .await
        .expect("generate_id() should be available after HeeRanjID setup");

    let id: i64 = row
        .try_get::<_, i64>("id")
        .expect("id column should decode as i64");

    assert!(
        id > 0,
        "generate_id() should return a positive HeerId; got {id}"
    );
}
