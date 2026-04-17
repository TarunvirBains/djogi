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
// CRUD tests (Task 7)
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn create_returns_full_row_with_generated_id(pool: PgPool) {
    setup_posts(&pool).await;

    let post = Post::create(
        &pool,
        Post {
            title: "My First Post".into(),
            body: "Hello, Djogi!".into(),
            published: false,
            view_count: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    assert!(
        post.id.as_i64() > 0,
        "id must be DB-generated positive HeerId"
    );
    assert_eq!(post.title, "My First Post");
    assert_eq!(post.body, "Hello, Djogi!");
    assert!(!post.published);
}

#[sqlx::test]
async fn get_returns_correct_row(pool: PgPool) {
    setup_posts(&pool).await;

    let created = Post::create(
        &pool,
        Post {
            title: "Fetchable Post".into(),
            body: "Body text".into(),
            published: true,
            view_count: 42,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    let fetched = Post::get(&pool, created.id)
        .await
        .expect("get should succeed");

    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.title, "Fetchable Post");
    assert_eq!(fetched.view_count, 42);
    assert!(fetched.published);
}

#[sqlx::test]
async fn get_returns_not_found_for_missing_id(pool: PgPool) {
    setup_posts(&pool).await;

    let missing_id =
        ::heeranjid::HeerId::from_i64(999_999_999).expect("999_999_999 is a valid HeerId");
    let result = Post::get(&pool, missing_id).await;

    assert!(
        matches!(result, Err(DjogiError::NotFound)),
        "expected NotFound, got {:?}",
        result
    );
}

#[sqlx::test]
async fn save_updates_fields(pool: PgPool) {
    setup_posts(&pool).await;

    let mut post = Post::create(
        &pool,
        Post {
            title: "Original Title".into(),
            body: "Original body".into(),
            published: false,
            view_count: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    post.title = "Updated Title".into();
    post.published = true;
    post.save(&pool).await.expect("save should succeed");

    let reloaded = Post::get(&pool, post.id).await.expect("get should succeed");
    assert_eq!(reloaded.title, "Updated Title");
    assert!(reloaded.published);
    assert_eq!(reloaded.body, "Original body");
}

#[sqlx::test]
async fn delete_removes_row(pool: PgPool) {
    setup_posts(&pool).await;

    let post = Post::create(
        &pool,
        Post {
            title: "To Be Deleted".into(),
            body: "Gone soon".into(),
            published: false,
            view_count: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    let id = post.id;
    post.delete(&pool).await.expect("delete should succeed");

    let result = Post::get(&pool, id).await;
    assert!(
        matches!(result, Err(DjogiError::NotFound)),
        "expected NotFound after delete, got {:?}",
        result
    );
}

#[sqlx::test]
async fn refresh_from_db_returns_current_state(pool: PgPool) {
    setup_posts(&pool).await;

    let post = Post::create(
        &pool,
        Post {
            title: "Before Refresh".into(),
            body: "Stale body".into(),
            published: false,
            view_count: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    // Simulate an out-of-band update (e.g. from another process).
    sqlx::query("UPDATE posts SET title = $1 WHERE id = $2")
        .bind("After Refresh")
        .bind(post.id.as_i64())
        .execute(&pool)
        .await
        .expect("out-of-band update should succeed");

    // Our in-memory `post` is stale — refresh_from_db should return the new state.
    let refreshed = post
        .refresh_from_db(&pool)
        .await
        .expect("refresh_from_db should succeed");

    assert_eq!(refreshed.title, "After Refresh");
    assert_eq!(refreshed.body, "Stale body");
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

    // Exactly 4 user fields — no framework-field leakage into the descriptor.
    assert_eq!(
        post_desc.fields.len(),
        4,
        "descriptor must contain exactly 4 user fields, got {}",
        post_desc.fields.len()
    );

    // Per-field sql_type + nullable spot-checks covering each mapping branch.
    let field = |name: &str| {
        post_desc
            .fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("field `{name}` missing from descriptor"))
    };
    let title = field("title");
    assert!(matches!(title.sql_type, ::djogi::FieldSqlType::Text));
    assert!(!title.nullable);
    let view_count = field("view_count");
    assert!(matches!(
        view_count.sql_type,
        ::djogi::FieldSqlType::Integer
    ));
    assert!(!view_count.nullable);
    let published = field("published");
    assert!(matches!(published.sql_type, ::djogi::FieldSqlType::Boolean));
    assert!(!published.nullable);

    // Per-field Phase 1 defaults on every user field.
    for f in post_desc.fields {
        assert!(
            f.rationale.is_none(),
            "field `{}` should have no rationale in Phase 1",
            f.name
        );
        assert!(
            !f.outbox_exclude,
            "field `{}` outbox_exclude should be false in Phase 1",
            f.name
        );
        assert!(
            f.index_type.is_none(),
            "field `{}` index_type should be None in Phase 1",
            f.name
        );
    }

    // Model-level Phase 1 defaults: all amended fields zero/None/empty.
    assert!(post_desc.partition_by.is_none());
    assert!(!post_desc.has_outbox);
    assert!(post_desc.idempotency_key.is_none());
    assert!(post_desc.tenant_key.is_none());
    assert!(post_desc.cache_ttl.is_none());
    assert!(post_desc.rationale.is_none());
    assert!(post_desc.indexes.is_empty());
}
