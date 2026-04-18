//! Phase 2 QuerySet integration tests.
//!
//! Task 5 only verifies that the builder compiles — `T::objects()` resolves,
//! `.filter(|f| ...)`, `.exclude(...)`, `.order_by(...)`, `.limit(n)`,
//! `.offset(n)`, `.distinct()`, `.distinct_on(|f| ...)`, and `.none()` all
//! type-check against a real `#[model]`-derived struct. Terminal methods
//! (`fetch_all`, `fetch_one`, `count`, `exists`, `first`, `update`, `delete`)
//! land in Task 6, and Tasks 6–9 will expand this file to exercise them.
//!
//! The `#[sqlx::test]` fixture takes a `PgPool` so running it requires a
//! live Postgres (same pattern as the Phase 1 smoke tests). When no database
//! is configured the test is skipped at fixture-wiring time — the part that
//! matters for Task 5 is that the *compilation* succeeds.

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
    // associated function on `QuerySet` (it constructs rather than
    // transforms), so it is called via `QuerySet::<Post>::none()`.
    let _empty: QuerySet<Post> = QuerySet::none();
    let _distinct = Post::objects().distinct();
    let _distinct_on = Post::objects().distinct_on(|f| f.title());
}
