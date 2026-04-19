> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Getting Started

This guide walks through setting up a Djogi project from scratch, defining
your first model, creating the table, and performing CRUD operations — from
a blank directory to a passing integration test.

Djogi is Postgres-only. The framework fields (`id`, `created_at`,
`updated_at`) are injected by the proc macro — you define only the fields
you own.

> **Phase 1 scope:** The CLI (`cargo djogi`) and the migration differ do not
> exist yet. Tables are created manually for now. The `QuerySet` filter API
> is a Phase 2 deliverable. This guide covers everything that ships in Phase 1.

---

## 1. Workspace Setup

### Prerequisites

- Rust toolchain (stable, 1.87 or later)
- PostgreSQL 18 running locally or via Docker

### Docker Compose quickstart

```yaml
# docker-compose.yml
services:
  postgres:
    image: postgres:18
    ports:
      - "5432:5432"
    environment:
      POSTGRES_USER: djogi
      POSTGRES_PASSWORD: djogi
      POSTGRES_DB: myapp
    volumes:
      - pgdata:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U djogi"]
      interval: 2s
      timeout: 5s
      retries: 10

volumes:
  pgdata:
```

```bash
docker compose up -d
export DATABASE_URL="postgres://djogi:djogi@localhost/myapp"
```

### Cargo.toml

```toml
[dependencies]
djogi = { path = "../../djogi" }  # path dep until published

sqlx = { version = "0.8", default-features = false, features = [
    "postgres",
    "runtime-tokio-rustls",
    "macros",
    "time",
    "uuid",
    "json",
] }

tokio = { version = "1", features = ["macros", "rt-multi-thread"] }

serde = { version = "1", features = ["derive"] }
serde_json = "1"

time = { version = "0.3", features = ["serde", "formatting", "parsing"] }
uuid = { version = "1", features = ["serde"] }

heeranjid = "0.1"
heeranjid-sqlx = "0.1"
```

> **Note:** Djogi uses the `time` crate for all datetime types — not
> `chrono`. Do not add `chrono` as a dependency.

---

## 2. First Model

Create your model in `src/models.rs`:

```rust
// src/models.rs
use djogi::prelude::*;

#[model(table = "articles")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Article {
    pub title: String,
    pub slug: String,
    pub body: String,
    pub published: bool,
    pub view_count: i32,
}
```

After `#[model]` expands, the struct effectively gains three injected fields:

```rust
// What the struct looks like after macro expansion (not written by hand):
pub struct Article {
    pub id: HeerId,                        // BIGINT DEFAULT generate_id(), injected PK
    pub created_at: time::OffsetDateTime,  // TIMESTAMPTZ DEFAULT now(), injected
    pub updated_at: time::OffsetDateTime,  // TIMESTAMPTZ DEFAULT now(), injected

    // Developer-defined fields:
    pub title: String,
    pub slug: String,
    pub body: String,
    pub published: bool,
    pub view_count: i32,
}
```

The macro also generates:

- `impl Model for Article` — `create`, `get`, `save`, `delete`, `refresh_from_db`
- `impl FromRow for Article` — SQLx row deserialization
- `Article::descriptor()` — submitted via `inventory` for app registration

---

## 3. Install HeeRanjId Schema

HeeRanjId provides the `generate_id()` Postgres function used by the `id`
column default. Install it once per database before creating any tables:

```rust
use sqlx::PgPool;

async fn prepare_db(pool: &PgPool) -> Result<(), sqlx::Error> {
    // Install the generate_id() and related functions
    heeranjid_sqlx::install_schema(pool).await?;
    // Seed node 1 — required for ID generation to work
    heeranjid_sqlx::seed_default_node(pool).await?;
    Ok(())
}
```

In integration tests, this is handled by the `setup_*` helper functions
that call both before creating any table. See the pattern used in
`tests/integration/phase1_model.rs`.

---

## 4. Create the Table

In Phase 1, tables are created manually. Match the column list to the model
definition, including the injected framework columns:

```rust
use sqlx::PgPool;

async fn create_articles_table(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE articles (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            title       TEXT        NOT NULL,
            slug        TEXT        NOT NULL,
            body        TEXT        NOT NULL,
            published   BOOLEAN     NOT NULL,
            view_count  INTEGER     NOT NULL
        )"
    )
    .execute(pool)
    .await?;
    Ok(())
}
```

The column order does not matter — `FromRow` matches by name.

---

## 5. CRUD

With the table created, use the generated Model trait methods:

### Create

```rust
use djogi::prelude::*;
use sqlx::PgPool;

async fn create_article(ctx: &mut DjogiContext) -> djogi::Result<Article> {
    let article = Article::create(ctx, Article {
        title: "Getting Started with Djogi".into(),
        slug: "getting-started".into(),
        body: "Djogi is a Model-first ORM for Rust...".into(),
        published: false,
        view_count: 0,
        // Framework fields — use ..Default::default().
        // The macro replaces them before the INSERT regardless.
        ..Default::default()
    }).await?;

    // id is populated by RETURNING after INSERT
    println!("Created article: {}", article.id);
    Ok(article)
}
```

Generated SQL:
```sql
INSERT INTO articles (title, slug, body, published, view_count)
VALUES ($1, $2, $3, $4, $5)
RETURNING id, created_at, updated_at, title, slug, body, published, view_count
```

### Fetch by primary key

```rust
async fn fetch_article(ctx: &mut DjogiContext, id: HeerId) -> djogi::Result<Article> {
    let article = Article::get(ctx, id).await?;
    println!("{}: {}", article.id, article.title);
    Ok(article)
}
```

Returns `Err(DjogiError::NotFound)` when no row matches.

### Update

```rust
async fn publish_article(ctx: &mut DjogiContext, id: HeerId) -> djogi::Result<()> {
    let mut article = Article::get(ctx, id).await?;
    article.published = true;
    article.view_count += 1;
    // Issues a full-row UPDATE; updated_at is refreshed automatically
    article.save(ctx).await?;
    Ok(())
}
```

### Delete

```rust
async fn remove_article(ctx: &mut DjogiContext, id: HeerId) -> djogi::Result<()> {
    let article = Article::get(ctx, id).await?;
    article.delete(ctx).await?;
    // article is moved — cannot be used after this point
    Ok(())
}
```

### Refresh from DB

```rust
async fn reload(ctx: &mut DjogiContext, article: Article) -> djogi::Result<Article> {
    // Returns a fresh copy of the row — useful after out-of-band DB changes
    let fresh = article.refresh_from_db(ctx).await?;
    Ok(fresh)
}
```

---

## 6. Raw SQL Escape Hatch

For queries the Model trait doesn't cover — aggregates, joins, custom
projections — use `djogi::raw::*`:

```rust
use djogi::prelude::*;

// query_as — returns a Vec of typed rows
let articles: Vec<Article> = djogi::raw::query_as(
    &mut ctx,
    "SELECT * FROM articles WHERE published = $1",
    |q| q.bind(true),
).await?;

// query_scalar — returns a single scalar value
let count: i64 = djogi::raw::query_scalar(
    &mut ctx,
    "SELECT COUNT(*) FROM articles",
    |q| q,
).await?;

// execute — runs a statement without returning rows
djogi::raw::execute(
    &mut ctx,
    "UPDATE articles SET view_count = view_count + 1 WHERE id = $1",
    |q| q.bind(article_id.as_i64()),
).await?;
```

All three functions take `&mut DjogiContext` — the same call site works
against a pool-backed context or a transaction-backed one:

```rust
let mut ctx = DjogiContext::from_pool(pool.clone());
let count: i64 = djogi::raw::query_scalar(
    &mut ctx,
    "SELECT COUNT(*) FROM articles",
    |q| q,
).await?;
```

---

## 7. Transactions

Model CRUD methods take `&mut DjogiContext`. For a transaction, construct
a tx-backed context and call `.commit()` / `.rollback()` when done:

```rust
async fn transfer_views(pool: &PgPool, from_id: HeerId, to_id: HeerId) -> djogi::Result<()> {
    let tx = pool.begin().await?;
    let mut tx_ctx = DjogiContext::from_transaction(tx);

    let mut source = Article::get(&mut tx_ctx, from_id).await?;
    let mut dest = Article::get(&mut tx_ctx, to_id).await?;

    dest.view_count += source.view_count;
    source.view_count = 0;

    source.save(&mut tx_ctx).await?;
    dest.save(&mut tx_ctx).await?;

    tx_ctx.commit().await?;
    Ok(())
}
```

If either `save()` fails, drop `tx_ctx` (or call `tx_ctx.rollback().await?`)
and neither row is modified in the DB. Phase 4 Task 1's `atomic()` wrapper
will layer on top of this to also manage on-commit callbacks and savepoint
nesting automatically.

---

## 8. First Test

Use `#[sqlx::test]` for integration tests. Each test gets an isolated
throwaway database — no shared state, no teardown ceremony.

```rust
// tests/integration/articles.rs
use djogi::prelude::*;
use sqlx::PgPool;

// Reuse this pattern for every test module that needs articles
async fn setup(pool: &PgPool) {
    heeranjid_sqlx::install_schema(pool).await.unwrap();
    heeranjid_sqlx::seed_default_node(pool).await.unwrap();

    // Persist the node ID at the DB level so all pool connections inherit it
    let db_name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(pool).await.unwrap();
    sqlx::query(&format!(
        "ALTER DATABASE \"{db_name}\" SET heer.node_id = '1'"
    ))
    .execute(pool).await.unwrap();
    sqlx::query("SELECT set_heer_node_id(1)")
        .execute(pool).await.unwrap();

    sqlx::query(
        "CREATE TABLE articles (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            title       TEXT        NOT NULL,
            slug        TEXT        NOT NULL,
            body        TEXT        NOT NULL,
            published   BOOLEAN     NOT NULL,
            view_count  INTEGER     NOT NULL
        )"
    )
    .execute(pool).await.unwrap();
}

#[sqlx::test]
async fn create_and_get(pool: PgPool) {
    setup(&pool).await;
    let mut ctx = DjogiContext::from_pool(pool.clone());

    let article = Article::create(&mut ctx, Article {
        title: "Test Article".into(),
        slug: "test".into(),
        body: "Body text".into(),
        published: false,
        view_count: 0,
        ..Default::default()
    })
    .await
    .expect("create should succeed");

    // id is DB-generated — positive and non-zero
    assert!(article.id.as_i64() > 0);
    assert_eq!(article.title, "Test Article");
    assert!(!article.published);

    // Fetch back by PK
    let fetched = Article::get(&mut ctx, article.id).await.unwrap();
    assert_eq!(fetched.slug, "test");
}

#[sqlx::test]
async fn save_updates_fields(pool: PgPool) {
    setup(&pool).await;
    let mut ctx = DjogiContext::from_pool(pool.clone());

    let mut article = Article::create(&mut ctx, Article {
        title: "Draft".into(),
        slug: "draft".into(),
        body: "".into(),
        published: false,
        view_count: 0,
        ..Default::default()
    })
    .await
    .unwrap();

    article.published = true;
    article.title = "Published".into();
    article.save(&mut ctx).await.unwrap();

    let reloaded = Article::get(&mut ctx, article.id).await.unwrap();
    assert!(reloaded.published);
    assert_eq!(reloaded.title, "Published");
}

#[sqlx::test]
async fn delete_removes_row(pool: PgPool) {
    setup(&pool).await;
    let mut ctx = DjogiContext::from_pool(pool.clone());

    let article = Article::create(&mut ctx, Article {
        title: "To Delete".into(),
        slug: "to-delete".into(),
        body: "".into(),
        published: false,
        view_count: 0,
        ..Default::default()
    })
    .await
    .unwrap();

    let id = article.id;
    article.delete(&mut ctx).await.unwrap();

    assert!(matches!(
        Article::get(&mut ctx, id).await,
        Err(DjogiError::NotFound)
    ));
}
```

Run the tests:

```bash
# Requires DATABASE_URL set to a Postgres 18 instance
cargo test -p djogi --test phase1_model -- --test-threads=1
```

The `--test-threads=1` flag is required when tests share a Postgres instance
to avoid conflicting `ALTER DATABASE` calls across simultaneous test runs.
When each test gets a fully isolated database (the default with `#[sqlx::test]`
and a fresh pool), parallel execution is safe.

---

## What's Next

- [Models guide](./models.md) — all `#[model(...)]` and `#[field(...)]`
  attributes available in Phase 1, including alternate PK types (`serial`,
  `ranjid`) and rich field types (`Decimal`, `Vec<T>`, `time::Date`).
- [Agent guide](./agent-guide.md) — if you are an AI coding agent working
  in a Djogi codebase, start here.
- [Roadmap](../roadmap/index.md) — planned features including QuerySet
  filters (Phase 2), RLS / tenant isolation (Phase 5+), and the
  `cargo djogi` CLI (Phase 6–8).
