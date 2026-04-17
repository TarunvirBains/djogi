//! Phase 1 integration tests. Uses throwaway test models — not framework types.

// Suppress result_large_err: figment::Error is large but external.
#![allow(clippy::result_large_err)]

use djogi::prelude::*;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// Test models — HeerId (default PK)
// ---------------------------------------------------------------------------

#[model(table = "posts")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Post {
    pub title: String,
    pub body: String,
    pub published: bool,
    pub view_count: i32,
}

// ---------------------------------------------------------------------------
// DB setup helpers
// ---------------------------------------------------------------------------

/// Install HeeRanjId schema + seed node 1 + create the posts table.
async fn setup_posts(pool: &PgPool) {
    heeranjid_sqlx::install_schema(pool).await.unwrap();
    heeranjid_sqlx::seed_default_node(pool).await.unwrap();
    // Set the session-level node ID so `generate_id()` (no-arg form) can
    // resolve `current_heer_node_id()` when executing DEFAULT column values.
    sqlx::query("SELECT set_heer_node_id(1)")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE posts (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            title       TEXT        NOT NULL,
            body        TEXT        NOT NULL,
            published   BOOLEAN     NOT NULL,
            view_count  INTEGER     NOT NULL
        )",
    )
    .execute(pool)
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// FromRow test (Task 5)
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn from_row_deserializes_correctly(pool: PgPool) {
    setup_posts(&pool).await;

    // Insert a row manually, then fetch it — tests FromRow in isolation
    let row = sqlx::query_as::<_, Post>(
        "INSERT INTO posts (title, body, published, view_count)
         VALUES ($1, $2, $3, $4)
         RETURNING *",
    )
    .bind("Hello World")
    .bind("First post body")
    .bind(false)
    .bind(0i32)
    .fetch_one(&pool)
    .await
    .expect("INSERT + FromRow should succeed");

    assert_eq!(row.title, "Hello World");
    assert_eq!(row.body, "First post body");
    assert!(!row.published);
    assert_eq!(row.view_count, 0);
    assert!(row.id.as_i64() > 0, "DB-generated HeerId must be positive");
}

// ---------------------------------------------------------------------------
// ModelDescriptor registration test (Task 6)
// ---------------------------------------------------------------------------

#[test]
fn model_descriptor_registered() {
    // inventory collects all ModelDescriptor submissions at link time.
    // `inventory::iter::<T>` is a zero-sized type implementing IntoIterator —
    // use it WITHOUT parentheses (not `inventory::iter::<T>()`).
    let descriptors: Vec<&::djogi::ModelDescriptor> = ::inventory::iter::<::djogi::ModelDescriptor>
        .into_iter()
        .collect();

    let post_desc = descriptors
        .iter()
        .find(|d| d.table_name == "posts")
        .expect("Post ModelDescriptor should be registered via inventory");

    assert_eq!(post_desc.type_name, "Post");
    assert_eq!(post_desc.table_name, "posts");
    assert!(matches!(post_desc.pk_type, ::djogi::PkType::HeerId));
    assert!(post_desc.fields.iter().any(|f| f.name == "title"));
    // Phase 1 defaults: all amended fields zero/None/empty.
    assert!(post_desc.partition_by.is_none());
    assert!(!post_desc.has_outbox);
    assert!(post_desc.idempotency_key.is_none());
    assert!(post_desc.tenant_key.is_none());
    assert!(post_desc.cache_ttl.is_none());
    assert!(post_desc.rationale.is_none());
    assert!(post_desc.indexes.is_empty());
}
