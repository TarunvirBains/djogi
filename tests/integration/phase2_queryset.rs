//! Phase 2 QuerySet integration tests.
//!
//! Task 5 covers the lazy-builder compile surface (see
//! `objects_returns_empty_queryset` below). Task 6 adds the terminal-method
//! exercises against live Postgres via `#[sqlx::test]` — `fetch_all`,
//! `fetch_one` (NotFound / MultipleObjects), `first`, `count`, `exists`,
//! and the `none()` short-circuit contract. Tasks 7–9 will grow this file
//! with writes (`update`, `delete`) and programmatic filters.
//!
//! The `#[sqlx::test]` fixture takes a `PgPool` so running it requires a
//! live Postgres (same pattern as the Phase 1 smoke tests). When no database
//! is configured the test is skipped at fixture-wiring time — the part that
//! matters for the Task 5 compile-only baseline is that the test file
//! *compiles*.

use djogi::prelude::*;
use sqlx::PgPool;

// Separate table name (`posts_p2`) so this integration test can share a DB
// with `phase1_model.rs` without DDL collisions.
#[model(table = "posts_p2")]
#[derive(Debug, Clone)]
pub struct Post {
    pub title: String,
    pub body: String,
    pub published: bool,
    pub view_count: i32,
}

/// Install HeeRanjId schema + seed node 1 + create `posts_p2`. Mirrors
/// `phase1_model::setup_posts` intentionally — each integration test file
/// owns its own setup so tests can be run in isolation. Factoring the
/// helper across files would couple test fixtures in a way that obscures
/// which test owns which DDL.
async fn setup(pool: &PgPool) {
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
    sqlx::query("SELECT set_heer_node_id(1)")
        .execute(pool)
        .await
        .unwrap();

    sqlx::query(
        "CREATE TABLE posts_p2 (
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

#[sqlx::test]
async fn objects_returns_empty_queryset(pool: PgPool) {
    setup(&pool).await;

    // `T::objects()` resolves and returns a lazy builder. No SQL has been
    // emitted or executed — Task 5 deliberately ships without terminal
    // methods.
    let qs = Post::objects();

    // Builder methods compose without executing. The clone proves
    // `QuerySet: Clone` (important for `if/else` branches that reuse a
    // partially-built queryset).
    let _qs2 = qs.clone().filter(|f| f.published().eq(true)).limit(10);

    // `exclude` + chained `order_by` + `offset` — covers the rest of the
    // Task 5 builder surface at compile time.
    let _qs3 = qs
        .clone()
        .exclude(|f| f.title().eq("draft".to_string()))
        .order_by(|f| f.view_count().desc())
        .order_by(|f| f.title().asc())
        .offset(5);

    // `none()` short-circuit branch + `distinct`/`distinct_on` — closes out
    // every public QuerySet method the task introduces. `none()` is an
    // instance method (matching Django's `queryset.none()` ergonomics) so
    // both `Post::objects().none()` and from-scratch construction via
    // `QuerySet::<Post>::new().none()` compile.
    let _empty_from_objects: QuerySet<Post> = Post::objects().none();
    let _empty_from_scratch: QuerySet<Post> = QuerySet::<Post>::new().none();
    let _distinct = Post::objects().distinct();
    let _distinct_on = Post::objects().distinct_on(|f| f.title());
}

// ── Task 6: terminal read methods ─────────────────────────────────────────
//
// Each test seeds four deterministic rows via `seed_posts` and then
// exercises exactly one terminal on a filter/composition that surfaces a
// distinct branch of the SQL emitter or terminal-method contract.

/// Seed four deterministic posts. Wraps every INSERT in a single
/// transaction so that `set_heer_node_id(1)` (required for `generate_id()`'s
/// column default) lands on the SAME connection that runs the INSERT —
/// `sqlx::test` provisions a multi-connection pool, so a bare
/// `SELECT set_heer_node_id(1)` on `&pool` only sets one random connection's
/// session variable and later INSERTs may hit a different connection. The
/// same pattern is used by Phase 1 tests (see `raw_execute_runs_without_return`
/// and friends in `phase1_model.rs`).
async fn seed_posts(pool: &PgPool) {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_heer_node_id(1)")
        .execute(&mut *tx)
        .await
        .unwrap();
    for (title, published, views) in [
        ("alpha", true, 100i32),
        ("beta", true, 50),
        ("gamma", false, 200),
        ("delta", true, 25),
    ] {
        sqlx::query(
            "INSERT INTO posts_p2 (title, body, published, view_count) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(title)
        .bind("body")
        .bind(published)
        .bind(views)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn fetch_all_no_filter(pool: PgPool) {
    setup(&pool).await;
    seed_posts(&pool).await;
    let rows = Post::objects().fetch_all(&pool).await.unwrap();
    assert_eq!(rows.len(), 4);
}

#[sqlx::test]
async fn fetch_all_with_filter(pool: PgPool) {
    setup(&pool).await;
    seed_posts(&pool).await;
    let rows = Post::objects()
        .filter(|f| f.published().eq(true))
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
}

#[sqlx::test]
async fn fetch_one_exact_match(pool: PgPool) {
    setup(&pool).await;
    seed_posts(&pool).await;
    let row = Post::objects()
        .filter(|f| f.title().eq("alpha".to_string()))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.title, "alpha");
}

#[sqlx::test]
async fn fetch_one_zero_rows_is_not_found(pool: PgPool) {
    setup(&pool).await;
    seed_posts(&pool).await;
    let err = Post::objects()
        .filter(|f| f.title().eq("nonexistent".to_string()))
        .fetch_one(&pool)
        .await
        .unwrap_err();
    assert!(matches!(err, DjogiError::NotFound { .. }));
}

#[sqlx::test]
async fn fetch_one_multiple_rows_is_multiple_objects(pool: PgPool) {
    setup(&pool).await;
    seed_posts(&pool).await;
    let err = Post::objects()
        .filter(|f| f.published().eq(true))
        .fetch_one(&pool)
        .await
        .unwrap_err();
    assert!(matches!(err, DjogiError::MultipleObjects { .. }));
}

#[sqlx::test]
async fn first_returns_some_or_none(pool: PgPool) {
    setup(&pool).await;
    seed_posts(&pool).await;
    let some = Post::objects()
        .filter(|f| f.published().eq(true))
        .first(&pool)
        .await
        .unwrap();
    assert!(some.is_some());

    let none = Post::objects()
        .filter(|f| f.title().eq("nope".to_string()))
        .first(&pool)
        .await
        .unwrap();
    assert!(none.is_none());
}

#[sqlx::test]
async fn count_returns_row_count(pool: PgPool) {
    setup(&pool).await;
    seed_posts(&pool).await;
    let n = Post::objects().count(&pool).await.unwrap();
    assert_eq!(n, 4);
    let n2 = Post::objects()
        .filter(|f| f.published().eq(true))
        .count(&pool)
        .await
        .unwrap();
    assert_eq!(n2, 3);
}

#[sqlx::test]
async fn exists_returns_bool(pool: PgPool) {
    setup(&pool).await;
    seed_posts(&pool).await;
    assert!(
        Post::objects()
            .filter(|f| f.title().eq("alpha".to_string()))
            .exists(&pool)
            .await
            .unwrap()
    );
    assert!(
        !Post::objects()
            .filter(|f| f.title().eq("nope".to_string()))
            .exists(&pool)
            .await
            .unwrap()
    );
}

#[sqlx::test]
async fn none_short_circuits_every_terminal(pool: PgPool) {
    setup(&pool).await;
    seed_posts(&pool).await;

    // `fetch_all` -> Ok(vec![])
    let empty = Post::objects().none().fetch_all(&pool).await.unwrap();
    assert!(empty.is_empty());

    // `count` -> Ok(0)
    assert_eq!(Post::objects().none().count(&pool).await.unwrap(), 0);

    // `exists` -> Ok(false)
    assert!(!Post::objects().none().exists(&pool).await.unwrap());

    // `first` -> Ok(None)
    assert!(Post::objects().none().first(&pool).await.unwrap().is_none());

    // `fetch_one` -> Err(NotFound)
    let none_err = Post::objects().none().fetch_one(&pool).await.unwrap_err();
    assert!(matches!(none_err, DjogiError::NotFound { .. }));
}

#[sqlx::test]
async fn limit_offset_paginate(pool: PgPool) {
    setup(&pool).await;
    seed_posts(&pool).await;
    let page1 = Post::objects()
        .order_by(|f| f.title().asc())
        .limit(2)
        .offset(0)
        .fetch_all(&pool)
        .await
        .unwrap();
    let page2 = Post::objects()
        .order_by(|f| f.title().asc())
        .limit(2)
        .offset(2)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(page1.len(), 2);
    assert_eq!(page2.len(), 2);
    assert_ne!(page1[0].title, page2[0].title);
}

#[sqlx::test]
async fn nested_and_or(pool: PgPool) {
    setup(&pool).await;
    seed_posts(&pool).await;
    let rows = Post::objects()
        .filter(|f| f.published().eq(true).and_with(f.view_count().gte(50i32)))
        .fetch_all(&pool)
        .await
        .unwrap();
    // alpha (views=100, published) + beta (views=50, published) match; delta
    // (views=25) and gamma (unpublished) do not.
    assert_eq!(rows.len(), 2);
}

#[sqlx::test]
async fn in_list_and_between(pool: PgPool) {
    setup(&pool).await;
    seed_posts(&pool).await;

    let by_title = Post::objects()
        .filter(|f| {
            f.title()
                .in_list(vec!["alpha".to_string(), "beta".to_string()])
        })
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(by_title.len(), 2);

    let by_views = Post::objects()
        .filter(|f| f.view_count().between(40i32, 120i32))
        .fetch_all(&pool)
        .await
        .unwrap();
    // alpha (100) + beta (50) fall inside [40, 120]; delta (25) and gamma
    // (200) do not.
    assert_eq!(by_views.len(), 2);
}

#[sqlx::test]
async fn distinct_on_and_plain(pool: PgPool) {
    setup(&pool).await;
    seed_posts(&pool).await;
    // Add duplicate titles so DISTINCT ON has real work to do. Same
    // transaction-wrap rationale as `seed_posts` — keeps
    // `set_heer_node_id(1)` and the INSERT on the same pool connection.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_heer_node_id(1)")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO posts_p2 (title, body, published, view_count) \
         VALUES ('dup', 'x', true, 1), ('dup', 'y', true, 2)",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let rows = Post::objects()
        .distinct_on(|f| f.title())
        .order_by(|f| f.title().asc())
        .fetch_all(&pool)
        .await
        .unwrap();
    // DISTINCT ON (title) keeps exactly one row per distinct title —
    // 'dup' collapses from 2 rows to 1.
    assert_eq!(
        rows.iter().filter(|p| p.title == "dup").count(),
        1,
        "distinct_on(title) should collapse duplicate titles"
    );
}
