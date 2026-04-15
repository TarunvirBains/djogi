//! Smoke test: verify we can connect to Postgres and run a basic query.

use sqlx::PgPool;

#[sqlx::test]
async fn connects_to_postgres(pool: PgPool) {
    let row: (i32,) = sqlx::query_as("SELECT 1")
        .fetch_one(&pool)
        .await
        .expect("failed to run SELECT 1");
    assert_eq!(row.0, 1);
}

#[sqlx::test]
async fn postgres_version_is_16(pool: PgPool) {
    let row: (String,) = sqlx::query_as("SELECT version()")
        .fetch_one(&pool)
        .await
        .expect("failed to get version");
    assert!(
        row.0.contains("PostgreSQL 16"),
        "Expected PostgreSQL 16, got: {}",
        row.0
    );
}
