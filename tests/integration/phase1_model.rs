//! Phase 1 integration tests. Uses throwaway test models — not framework types.
// `from_sqlx_pool_for_test` is a deprecated T2–T9 bridge; every call site in this
// file is an intended consumer. Suppress the lint rather than re-adding `#[allow]`
// to each of the many call sites individually.
#![allow(deprecated)]

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
///
/// Uses `ALTER DATABASE ... SET heer.node_id = '1'` to persist the node ID
/// at the database level so every connection in the sqlx::test pool inherits
/// it. A plain session-level `SELECT set_heer_node_id(1)` on `&pool` only
/// lands on whichever connection the pool happens to hand out — subsequent
/// pool-level queries may hit a different connection and fail with
/// "heer.node_id is not set for this session". ALTER DATABASE is idempotent
/// at the DB level and inherited by all subsequent connections.
async fn setup_posts(pool: &PgPool) {
    heeranjid_sqlx::install_schema(pool).await.unwrap();
    heeranjid_sqlx::seed_default_node(pool).await.unwrap();

    let db_name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "ALTER DATABASE \"{db_name}\" SET heer.node_id = '1'"
    ))
    .execute(pool)
    .await
    .unwrap();
    // Also SET the current session to cover any pool connection that was
    // already open before the ALTER DATABASE took effect.
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
    let mut ctx = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    setup_posts(&pool).await;

    let post = Post::create(
        &mut ctx,
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
    let mut ctx = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    setup_posts(&pool).await;

    let created = Post::create(
        &mut ctx,
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

    let fetched = Post::get(&mut ctx, created.id)
        .await
        .expect("get should succeed");

    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.title, "Fetchable Post");
    assert_eq!(fetched.view_count, 42);
    assert!(fetched.published);
}

#[sqlx::test]
async fn get_returns_not_found_for_missing_id(pool: PgPool) {
    let mut ctx = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    setup_posts(&pool).await;

    let missing_id =
        ::heeranjid::HeerId::from_i64(999_999_999).expect("999_999_999 is a valid HeerId");
    let result = Post::get(&mut ctx, missing_id).await;

    assert!(
        matches!(result, Err(DjogiError::NotFound { .. })),
        "expected NotFound, got {:?}",
        result
    );
}

#[sqlx::test]
async fn save_updates_fields(pool: PgPool) {
    let mut ctx = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    setup_posts(&pool).await;

    let mut post = Post::create(
        &mut ctx,
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
    post.save(&mut ctx).await.expect("save should succeed");

    let reloaded = Post::get(&mut ctx, post.id)
        .await
        .expect("get should succeed");
    assert_eq!(reloaded.title, "Updated Title");
    assert!(reloaded.published);
    assert_eq!(reloaded.body, "Original body");
}

#[sqlx::test]
async fn delete_removes_row(pool: PgPool) {
    let mut ctx = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    setup_posts(&pool).await;

    let post = Post::create(
        &mut ctx,
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
    post.delete(&mut ctx).await.expect("delete should succeed");

    let result = Post::get(&mut ctx, id).await;
    assert!(
        matches!(result, Err(DjogiError::NotFound { .. })),
        "expected NotFound after delete, got {:?}",
        result
    );
}

#[sqlx::test]
async fn refresh_from_db_returns_current_state(pool: PgPool) {
    let mut ctx = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    setup_posts(&pool).await;

    let post = Post::create(
        &mut ctx,
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
        .refresh_from_db(&mut ctx)
        .await
        .expect("refresh_from_db should succeed");

    assert_eq!(refreshed.title, "After Refresh");
    assert_eq!(refreshed.body, "Stale body");
}

// ---------------------------------------------------------------------------
// Serial PK model (Task 8)
// ---------------------------------------------------------------------------

#[model(table = "tags", pk = "serial")]
#[derive(Debug, Clone, PartialEq)]
pub struct Tag {
    pub name: String,
    pub color: String,
}

async fn setup_tags(pool: &PgPool) {
    sqlx::query(
        "CREATE TABLE tags (
            id         SERIAL      PRIMARY KEY,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            name       TEXT        NOT NULL,
            color      TEXT        NOT NULL
        )",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test]
async fn serial_pk_create_and_get(pool: PgPool) {
    let mut ctx = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    setup_tags(&pool).await;

    let tag = Tag::create(
        &mut ctx,
        Tag {
            name: "rust".into(),
            color: "#f74c00".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    assert!(tag.id > 0, "serial id must be positive");
    assert_eq!(tag.name, "rust");

    let fetched = Tag::get(&mut ctx, tag.id)
        .await
        .expect("get should find by i32 id");
    assert_eq!(fetched.name, "rust");
}

// ---------------------------------------------------------------------------
// RanjId PK model (Task 8)
// ---------------------------------------------------------------------------

#[model(table = "events", pk = "ranjid")]
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub kind: String,
    pub payload: String,
}

async fn setup_events(pool: &PgPool) {
    heeranjid_sqlx::install_schema(pool).await.unwrap_or(());
    heeranjid_sqlx::seed_default_node(pool).await.unwrap_or(());
    // generate_ranjid() uses current_heer_ranj_node_id() — a SEPARATE session
    // variable from heer.node_id. Persist at DB level so every pool
    // connection inherits it (same rationale as setup_posts).
    let db_name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "ALTER DATABASE \"{db_name}\" SET heer.ranj_node_id = '1'"
    ))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("SELECT set_heer_ranj_node_id(1)")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE events (
            id         UUID        PRIMARY KEY DEFAULT generate_ranjid(),
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            kind       TEXT        NOT NULL,
            payload    TEXT        NOT NULL
        )",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test]
async fn ranjid_pk_create_and_get(pool: PgPool) {
    let mut ctx = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    setup_events(&pool).await;

    let event = Event::create(
        &mut ctx,
        Event {
            kind: "user.signup".into(),
            payload: "{}".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    // RanjId wraps Uuid — check the underlying UUID isn't nil.
    assert!(!event.id.as_uuid().is_nil(), "RanjId must not be nil UUID");

    let fetched = Event::get(&mut ctx, event.id)
        .await
        .expect("get should find by RanjId");
    assert_eq!(fetched.kind, "user.signup");
}

// ---------------------------------------------------------------------------
// create_with_id + transaction tests (Task 9)
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn create_with_id_is_idempotent(pool: PgPool) {
    let mut ctx = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    setup_posts(&pool).await;

    // Simulate form pre-generation: allocate ID before user submits.
    // Query generate_id() directly via the DjogiContext to get a heeranjid 0.2.x HeerId.
    let id_row = ctx
        .__query_one_for_macros("SELECT generate_id() AS id", &[])
        .await
        .expect("generate_id() should succeed");
    let pre_generated_id: ::djogi::HeerId =
        ::djogi::HeerId::from_i64(id_row.try_get::<_, i64>("id").expect("id is i64"))
            .expect("generate_id() returns a valid HeerId");

    let post = Post::create_with_id(
        &mut ctx,
        pre_generated_id,
        Post {
            title: "Pre-generated".into(),
            body: "Idempotent insert".into(),
            published: false,
            view_count: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create_with_id should succeed");

    assert_eq!(
        post.id, pre_generated_id,
        "returned id must match pre-generated id"
    );

    // Second call with same id: ON CONFLICT DO NOTHING — should not error.
    // Phase 1 limitation: when the conflict fires, RETURNING * returns no rows,
    // and the method falls back to returning the caller-supplied value with the
    // given id (not the original row's data). Full idempotent fetch is deferred
    // to a later phase — see the DONE_WITH_CONCERNS note in crud.rs.
    let second = Post::create_with_id(
        &mut ctx,
        pre_generated_id,
        Post {
            title: "Different title".into(),
            body: "Different body".into(),
            published: true,
            view_count: 99,
            ..Default::default()
        },
    )
    .await
    .expect("idempotent re-insert should not error");

    assert_eq!(
        second.id, pre_generated_id,
        "id must match pre-generated id on conflict"
    );
}

#[sqlx::test]
async fn crud_respects_transaction_boundary(pool: PgPool) {
    let mut ctx = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    // Proves BOTH directions of the transaction boundary:
    //   (a) commit path  — Post::create'd row IS visible after commit
    //   (b) rollback path — Post::create'd row is NOT visible after rollback
    //
    // Earlier revision only tested (b) which is a false positive: an
    // uncommitted transaction's row wouldn't be visible to the pool's other
    // connections REGARDLESS of whether the txn rolled back or just dropped.
    // We need both branches to actually prove the boundary works.
    setup_posts(&pool).await;

    // (a) commit — insert + save inside txn, commit, row must be visible
    let _pc_commit = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    let mut tx_commit_ctx = _pc_commit.begin().await.unwrap();
    let committed = Post::create(
        &mut tx_commit_ctx,
        Post {
            title: "Committed".into(),
            body: "persists".into(),
            published: true,
            view_count: 1,
            ..Default::default()
        },
    )
    .await
    .expect("create inside commit txn should succeed");
    tx_commit_ctx.commit().await.unwrap();

    let fetched = Post::get(&mut ctx, committed.id)
        .await
        .expect("committed row must be visible");
    assert_eq!(fetched.title, "Committed");

    // (b) rollback — insert + save inside txn, rollback, row must NOT be visible
    let _pc_rollback = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    let mut tx_rollback_ctx = _pc_rollback.begin().await.unwrap();
    let mut rolled_back = Post::create(
        &mut tx_rollback_ctx,
        Post {
            title: "Rolled Back".into(),
            body: "does not persist".into(),
            published: false,
            view_count: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create inside rollback txn should succeed");
    rolled_back
        .save(&mut tx_rollback_ctx)
        .await
        .expect("save inside rollback txn should succeed");
    tx_rollback_ctx.rollback().await.unwrap();

    let result = Post::get(&mut ctx, rolled_back.id).await;
    assert!(
        matches!(result, Err(DjogiError::NotFound { .. })),
        "rolled-back row must NOT be visible, got: {:?}",
        result
    );
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

    // Phase 1.5: fields includes framework columns (id, created_at, updated_at)
    // plus the 4 user fields = 7 total for HeerId-PK models
    // (3 framework + 4 user = 7).
    assert_eq!(
        post_desc.fields.len(),
        7,
        "descriptor must contain id + created_at + updated_at + 4 user fields, got {}",
        post_desc.fields.len()
    );

    // Framework fields appear first, in injection order.
    assert_eq!(post_desc.fields[0].name, "id");
    assert!(
        matches!(post_desc.fields[0].sql_type, ::djogi::FieldSqlType::BigInt),
        "id sql_type must be BigInt for HeerId model"
    );
    assert!(!post_desc.fields[0].nullable);

    assert_eq!(post_desc.fields[1].name, "created_at");
    assert!(
        matches!(
            post_desc.fields[1].sql_type,
            ::djogi::FieldSqlType::Timestamptz
        ),
        "created_at sql_type must be Timestamptz"
    );
    assert!(!post_desc.fields[1].nullable);

    assert_eq!(post_desc.fields[2].name, "updated_at");
    assert!(
        matches!(
            post_desc.fields[2].sql_type,
            ::djogi::FieldSqlType::Timestamptz
        ),
        "updated_at sql_type must be Timestamptz"
    );
    assert!(!post_desc.fields[2].nullable);

    // Per-field sql_type + nullable spot-checks covering each mapping branch.
    // Uses find() so these still work regardless of the framework-fields prefix.
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

    // Per-field Phase 1 defaults on every field (including framework fields,
    // which also have rationale=None, outbox_exclude=false, index_type=None).
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

// ==========================================================================
// TASK 10 — rich field types (Decimal, Vec<T>, time::Date, Option<String>)
// ==========================================================================

use rust_decimal::Decimal;

// `no_default` suppresses the generated `Default` impl because `time::Date`
// does not implement `Default`. The test uses explicit field initialisation.
#[model(table = "products", no_default)]
#[derive(Debug, Clone, PartialEq)]
pub struct Product {
    pub name: String,
    pub price: Decimal,
    pub in_stock: bool,
    pub tags: Vec<String>,
    pub ratings: Vec<i32>,
    pub launch_date: ::time::Date,
    pub description: Option<String>,
}

async fn setup_products(pool: &PgPool) {
    heeranjid_sqlx::install_schema(pool).await.unwrap_or(());
    heeranjid_sqlx::seed_default_node(pool).await.unwrap_or(());
    // Persist heer.node_id at the database level so every new connection
    // (including DjogiPool connections) inherits it. Same rationale as
    // setup_posts — see that helper's doc comment for the full explanation.
    let db_name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "ALTER DATABASE \"{db_name}\" SET heer.node_id = '1'"
    ))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("SELECT set_heer_node_id(1)")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE products (
            id           BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
            name         TEXT        NOT NULL,
            price        NUMERIC     NOT NULL,
            in_stock     BOOLEAN     NOT NULL,
            tags         TEXT[]      NOT NULL,
            ratings      INTEGER[]   NOT NULL,
            launch_date  DATE        NOT NULL,
            description  TEXT
        )",
    )
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test]
async fn rich_field_types_roundtrip(pool: PgPool) {
    let mut ctx = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    setup_products(&pool).await;

    use rust_decimal_macros::dec;

    // Construct without ..Default::default() because time::Date does not
    // implement Default; all injected framework fields use sentinel values.
    let product = Product::create(
        &mut ctx,
        Product {
            id: ::djogi::types::__heerid_default(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            name: "Djogi Framework".into(),
            price: dec!(49.99),
            in_stock: true,
            tags: vec!["rust".into(), "framework".into()],
            ratings: vec![5, 4, 5],
            launch_date: ::time::Date::from_calendar_date(2026, ::time::Month::April, 15).unwrap(),
            description: Some("A Model-first web framework".into()),
        },
    )
    .await
    .expect("create with rich fields should succeed");

    assert_eq!(product.name, "Djogi Framework");
    assert_eq!(product.price, dec!(49.99));
    assert!(product.in_stock, "bool field must round-trip on create");
    assert_eq!(
        product.tags,
        vec!["rust".to_string(), "framework".to_string()]
    );
    assert_eq!(product.ratings, vec![5, 4, 5]);
    assert_eq!(
        product.description,
        Some("A Model-first web framework".into())
    );

    // Round-trip through FromRow — fetch the row and assert the decoded values match.
    let fetched = Product::get(&mut ctx, product.id)
        .await
        .expect("get should find product");
    assert_eq!(fetched.price, dec!(49.99));
    assert!(fetched.in_stock, "bool field must round-trip on FromRow");
    assert_eq!(
        fetched.launch_date,
        ::time::Date::from_calendar_date(2026, ::time::Month::April, 15).unwrap()
    );
    assert_eq!(fetched.tags.len(), 2);
    assert_eq!(fetched.ratings, vec![5, 4, 5]);
}

// ==========================================================================
// TASK 11 — djogi::raw escape-hatch API
// ==========================================================================

#[sqlx::test]
#[ignore = "djogi::raw is a todo!() stub in T2; re-enable in T5"]
async fn raw_query_as_returns_typed_models(pool: PgPool) {
    let mut ctx = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    setup_posts(&pool).await;

    Post::create(
        &mut ctx,
        Post {
            title: "Raw SQL Test".into(),
            body: "body".into(),
            published: true,
            view_count: 7,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // T5 note: bind_fn closure shape will change; placeholder type annotation for compile.
    let results: Vec<Post> = ::djogi::raw::query_as(
        &mut ctx,
        "SELECT * FROM posts WHERE published = $1",
        |_q: ()| (),
    )
    .await
    .expect("raw query_as should succeed");

    assert!(!results.is_empty(), "at least one published post expected");
    assert!(results.iter().all(|p| p.published));
}

#[sqlx::test]
#[ignore = "djogi::raw is a todo!() stub in T2; re-enable in T5"]
async fn raw_query_scalar_returns_count(pool: PgPool) {
    let mut ctx = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    setup_posts(&pool).await;

    Post::create(
        &mut ctx,
        Post {
            title: "Count Me".into(),
            body: "body".into(),
            published: true,
            view_count: 0,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // T5 note: bind_fn closure shape will change; placeholder type annotation for compile.
    let count: i64 =
        ::djogi::raw::query_scalar(&mut ctx, "SELECT COUNT(*) FROM posts", |_q: ()| ())
            .await
            .expect("count scalar should succeed");

    assert!(count >= 1);
}

// T5 deferred: djogi::raw::{query_scalar, execute} are todo!() stubs in T2.
// These three tests are marked #[ignore] until T5 implements the tokio-postgres
// raw API bodies. The test bodies are kept in-place so T5 only needs to un-ignore
// them (and remove the _pool parameter suppression where applicable).

#[sqlx::test]
#[ignore = "djogi::raw is a todo!() stub in T2; re-enable in T5"]
async fn raw_execute_runs_without_return(pool: PgPool) {
    setup_posts(&pool).await;
    let _pc = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    let mut tx_ctx = _pc.begin().await.unwrap();

    // T5 note: bind_fn closure shape will change; placeholder type annotations for compile.
    let before: i64 =
        ::djogi::raw::query_scalar::<i64, _>(&mut tx_ctx, "SELECT COUNT(*) FROM posts", |_: ()| ())
            .await
            .unwrap();

    ::djogi::raw::execute(
        &mut tx_ctx,
        "INSERT INTO posts (title, body, published, view_count) VALUES ($1, $2, $3, $4)",
        |_: ()| (),
    )
    .await
    .expect("raw execute should succeed");

    let after: i64 =
        ::djogi::raw::query_scalar::<i64, _>(&mut tx_ctx, "SELECT COUNT(*) FROM posts", |_: ()| ())
            .await
            .unwrap();
    assert_eq!(after, before + 1, "execute must insert exactly one row");
    tx_ctx.commit().await.unwrap();
}

#[sqlx::test]
#[ignore = "djogi::raw is a todo!() stub in T2; re-enable in T5"]
async fn raw_query_scalar_returns_not_found_for_empty_result(pool: PgPool) {
    let mut ctx = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    setup_posts(&pool).await;

    // T5 note: bind_fn closure shape will change; placeholder type annotation for compile.
    let result: Result<i64, ::djogi::DjogiError> = ::djogi::raw::query_scalar(
        &mut ctx,
        "SELECT view_count FROM posts WHERE id = $1",
        |_: ()| (), // T5: was |q| q.bind(-1i64)
    )
    .await;

    assert!(
        matches!(result, Err(::djogi::DjogiError::NotFound { .. })),
        "zero-row scalar must return DjogiError::NotFound, got: {:?}",
        result
    );
}

#[sqlx::test]
#[ignore = "djogi::raw is a todo!() stub in T2; re-enable in T5"]
async fn raw_works_inside_transaction(pool: PgPool) {
    let mut ctx = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    setup_posts(&pool).await;

    // T5 note: bind_fn closure shape will change; placeholder type annotations for compile.
    // --- (a) commit path ---
    let _pc_a = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    let mut tx_ctx = _pc_a.begin().await.unwrap();
    let before_commit: i64 =
        ::djogi::raw::query_scalar::<i64, _>(&mut tx_ctx, "SELECT COUNT(*) FROM posts", |_: ()| ())
            .await
            .unwrap();
    ::djogi::raw::execute(
        &mut tx_ctx,
        "INSERT INTO posts (title, body, published, view_count) VALUES ($1, $2, $3, $4)",
        |_: ()| (),
    )
    .await
    .expect("raw execute inside committed txn should succeed");
    tx_ctx.commit().await.unwrap();

    let after_commit: i64 =
        ::djogi::raw::query_scalar::<i64, _>(&mut ctx, "SELECT COUNT(*) FROM posts", |_: ()| ())
            .await
            .unwrap();
    assert_eq!(
        after_commit,
        before_commit + 1,
        "committed raw insert must be visible"
    );

    // --- (b) rollback path ---
    let _pc_b = ::djogi::DjogiContext::from_sqlx_pool_for_test(pool.clone())
        .await
        .unwrap();
    let mut tx_ctx = _pc_b.begin().await.unwrap();
    ::djogi::raw::execute(
        &mut tx_ctx,
        "INSERT INTO posts (title, body, published, view_count) VALUES ($1, $2, $3, $4)",
        |_: ()| (),
    )
    .await
    .expect("raw execute inside rollback txn should succeed");
    tx_ctx.rollback().await.unwrap();

    let after_rollback: i64 =
        ::djogi::raw::query_scalar::<i64, _>(&mut ctx, "SELECT COUNT(*) FROM posts", |_: ()| ())
            .await
            .unwrap();
    assert_eq!(
        after_rollback, after_commit,
        "rolled-back raw insert must NOT be visible"
    );
}
