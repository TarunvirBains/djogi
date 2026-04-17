> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Getting Started

This guide walks through setting up a Djogi project, defining your first model, running migrations, and performing CRUD operations — from a blank directory to a working application.

Djogi is Postgres-only. It generates all ORM code, migrations, and audit infrastructure from plain Rust structs. The framework fields (`id`, `created_at`, `updated_at`) are injected by the proc macro — you define only the fields you own.

---

## 1. Installation

### Prerequisites

- Rust toolchain (stable, 1.87 or later)
- PostgreSQL 16 or later running locally (or via Docker — see below)
- `cargo install djogi-cli` (the `cargo djogi` binary)

Install the CLI:

```bash
cargo install djogi-cli
```

### Scaffold a new project

```bash
cargo djogi new my-app
cd my-app
```

This creates the project layout, initializes the `migrations/` git submodule, and writes a starter `Djogi.toml`. If you are adding Djogi to an existing project, use `cargo djogi init` instead.

### Cargo.toml dependencies

For a new project, `cargo djogi new` writes these for you. For an existing project:

```toml
[dependencies]
djogi = "0.1"
djogi-macros = "0.1"

# SQLx — Postgres driver, connection pooling, row mapping
sqlx = { version = "0.8", default-features = false, features = [
    "postgres",
    "runtime-tokio-rustls",
    "macros",
    "time",
    "uuid",
    "json",
] }

# Async runtime
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }

# Serialization — needed for shell, API responses, JSONB fields
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Datetime — Djogi uses `time`, not `chrono`
time = { version = "0.3", features = ["serde", "formatting", "parsing"] }

# UUID — required for RanjId fields
uuid = { version = "1", features = ["serde"] }

# HeeRanjId — ID generation
heeranjid = "0.1"
heeranjid-sqlx = "0.1"

[build-dependencies]
# The build script runs the migration differ on every `cargo build`
djogi-build = "0.1"
```

### build.rs

Every Djogi project needs a `build.rs` at the project root. The build script reads model descriptors written by the proc macro during compilation and diffs them against `migrations/schema_snapshot.json` to generate migration files.

```rust
// build.rs
fn main() {
    djogi_build::run();
}
```

---

## 2. Environment Setup

Djogi separates secrets from configuration. Database URLs and the node identity go in environment variables. Non-secret configuration lives in `Djogi.toml`.

### Required environment variables

| Variable | Purpose | Example |
|---|---|---|
| `DATABASE_URL` | PostgreSQL connection string | `postgres://djogi:djogi@localhost/myapp` |
| `NODE_ID` | HeeRanjId node identifier (integer, registered in `heer_nodes`) | `1` |

> **Warning:** `NODE_ID` must exist as a registered row in the `heer_nodes` table before the application starts. Djogi validates this at startup and fails fast if the node is not found. For local development, running `cargo djogi db reset --seed` installs HeeRanjId's schema and seeds node 1 automatically.

For production, `DJOGI_ENV=production` activates additional safety guards (blocks `db reset`, enables snapshot signature verification, enforces stricter logging).

### Optional environment variables

| Variable | Purpose |
|---|---|
| `DJOGI_ENV` | `development` (default) or `production` |
| `DJOGI_SIGNING_KEY` | HMAC key for schema snapshot signing (required in production) |
| `CRUD_LOG_URL` | Connection string for the CRUD audit log database |
| `EVENT_LOG_URL` | Connection string for the event/observability log database |

### Djogi.toml

```toml
[database]
url = "postgres://localhost/myapp"   # overridden by DATABASE_URL env var
max_connections = 10
dev_mode = false                     # must be true to use `cargo djogi db reset`

[server]
host = "0.0.0.0"
port = 8000

[migrations]
submodule = "migrations"
allow_destructive = false            # require --allow-destructive flag for DROP operations

[shell]
history_file = ".djogi_history"
transaction_timeout_default = "30m"
scripts_dir = "scripts"
error_log_dir = ".djogi_shell_errors"
error_log_retention = "1y"

[features]
dirty_tracking = false               # opt in to per-field dirty tracking
```

The `DATABASE_URL` environment variable always overrides `[database].url`. Never put secrets in `Djogi.toml` — it is committed to version control.

### Docker Compose quickstart

For local development, start Postgres with Docker Compose:

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

Start Postgres and set your environment:

```bash
docker compose up -d

export DATABASE_URL="postgres://djogi:djogi@localhost/myapp"
export NODE_ID=1
export DJOGI_ENV=development
```

Initialize the database (installs HeeRanjId schema, runs migrations, seeds node 1):

```bash
# Djogi.toml must have dev_mode = true for this to work
cargo djogi db reset --seed
```

---

## 3. First Model

Create your application directory and define a model:

```
src/
  apps/
    posts/
      mod.rs
      models.rs
      routes.rs
  main.rs
```

```rust
// src/apps/posts/models.rs
use djogi::prelude::*;

#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[model(table = "posts")]
pub struct Post {
    pub title: String,
    pub slug: String,
    pub body: String,
    pub published: bool,
    pub view_count: i32,
    pub rating: Option<f64>,              // nullable DOUBLE PRECISION
    pub published_at: Option<time::OffsetDateTime>, // nullable TIMESTAMPTZ
}
```

After `#[derive(Model)]` expands, the struct effectively gains three injected fields:

```rust
// What the struct looks like after macro expansion (not written by hand)
pub struct Post {
    pub id: HeerId,                       // BIGINT DEFAULT generate_id(), injected PK
    pub created_at: time::OffsetDateTime, // TIMESTAMPTZ DEFAULT now(), injected
    pub updated_at: time::OffsetDateTime, // TIMESTAMPTZ DEFAULT now(), injected

    // Developer-defined fields:
    pub title: String,
    pub slug: String,
    pub body: String,
    pub published: bool,
    pub view_count: i32,
    pub rating: Option<f64>,
    pub published_at: Option<time::OffsetDateTime>,
}
```

The macro also generates:

- `impl Model for Post` — `objects()`, `get()`, `create()`, `save()`, `delete()`
- `impl FromRow for Post` — SQLx row deserialization
- `PostFields` — typed field accessors for filter closures
- `PostFilter` — programmatic filter builder for shell and dynamic use
- `Post::descriptor()` — submitted via `inventory` for app registration and the migration differ

Register the app so Djogi knows about it:

```rust
// src/apps/posts/mod.rs
use djogi::prelude::*;
use super::models::Post;

pub mod models;
pub mod routes;

struct PostsApp;

impl App for PostsApp {
    fn models() -> &'static [ModelDescriptor] {
        &[Post::descriptor()]
    }
    fn routes() -> axum::Router {
        routes::posts_router()
    }
}

djogi::register_app!(PostsApp);
```

---

## 4. First Migration

Build the project. The build script reads model descriptors and diffs them against the schema snapshot:

```bash
cargo build
```

On the first build with a new model, you will see a compiler-style diagnostic:

```
warning[D001]: schema drift detected — migration generated
  --> src/apps/posts/models.rs:5:1
   |
 5 | pub struct Post {
   | ^^^^^^^^^^^^^^^^ new table — no migration existed
   |
   = note: generated migrations/0001_create_posts_up.sql
   = note: generated migrations/0001_create_posts_down.sql
   = help: review the SQL, then run `cargo djogi migrate` when ready
```

Review the generated SQL before applying:

```bash
cargo djogi plan
```

```
Pending migrations:
  0001_create_posts   CREATE TABLE posts (6 columns + indexes)

Run `cargo djogi migrate` to apply.
```

Inspect the generated file:

```sql
-- migrations/0001_create_posts_up.sql
-- Migration: 0001_create_posts
-- Direction: UP
-- Generated: 2026-04-15T10:00:00Z

CREATE TABLE posts (
    id             BIGINT PRIMARY KEY DEFAULT generate_id(),
    title          TEXT NOT NULL,
    slug           TEXT NOT NULL,
    body           TEXT NOT NULL,
    published      BOOLEAN NOT NULL,
    view_count     INTEGER NOT NULL,
    rating         DOUBLE PRECISION,
    published_at   TIMESTAMPTZ,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

Apply the migration:

```bash
cargo djogi migrate
```

```
Applying 0001_create_posts... done
Schema snapshot updated to version 0001.
```

`schema_snapshot.json` is updated only when `cargo djogi migrate` succeeds. The snapshot is the source of truth for the differ — it represents what the database actually looks like, not what was last compiled.

---

## 5. First CRUD

With the migration applied, you can use the model in your application code.

### Create

```rust
use djogi::prelude::*;
use sqlx::PgPool;

async fn create_post(pool: &PgPool) -> djogi::Result<Post> {
    // Pass the struct directly. Framework-injected fields (id, created_at, updated_at)
    // are populated by the framework after the INSERT via RETURNING.
    let post = Post::create(pool, Post {
        title: "Getting Started with Djogi".into(),
        slug: "getting-started-with-djogi".into(),
        body: "Djogi is a Model-first ORM for Rust...".into(),
        published: false,
        view_count: 0,
        rating: None,
        published_at: None,
        // Framework fields — use Default::default() or leave them out with struct update syntax.
        // The macro replaces them before the INSERT regardless.
        ..Default::default()
    }).await?;

    println!("Created post with id: {}", post.id); // populated from RETURNING id
    Ok(post)
}
```

### Fetch by primary key

```rust
async fn fetch_post(pool: &PgPool, id: HeerId) -> djogi::Result<Post> {
    let post = Post::get(pool, id).await?;
    println!("{}: {}", post.id, post.title);
    Ok(post)
}
```

### Query with filters

```rust
async fn published_posts(pool: &PgPool) -> djogi::Result<Vec<Post>> {
    // QuerySet is lazy — nothing hits the DB until a terminal method is called.
    let posts = Post::objects()
        .filter(|f| f.published.eq(true))
        .order_by(|f| f.published_at.desc())
        .limit(20)
        .fetch_all(pool).await?;

    Ok(posts)
}
```

### Update

```rust
async fn publish_post(pool: &PgPool, id: HeerId) -> djogi::Result<()> {
    let mut post = Post::get(pool, id).await?;
    post.published = true;
    post.published_at = Some(time::OffsetDateTime::now_utc());
    // UPDATE posts SET published = $1, published_at = $2, updated_at = $3 WHERE id = $4
    post.save(pool).await?;
    Ok(())
}
```

### Delete

```rust
async fn delete_post(pool: &PgPool, id: HeerId) -> djogi::Result<()> {
    let post = Post::get(pool, id).await?;
    post.delete(pool).await?;
    Ok(())
}
```

### Axum handler

Djogi does not hide Axum. Handlers are standard Axum handlers. The pool comes from `State` extraction:

```rust
// src/apps/posts/routes.rs
use axum::{extract::{Path, State}, response::IntoResponse, Json};
use djogi::prelude::*;
use sqlx::PgPool;

pub async fn post_detail(
    State(pool): State<PgPool>,
    Path(id): Path<HeerId>,
) -> impl IntoResponse {
    match Post::get(&pool, id).await {
        Ok(post) => Json(post).into_response(),
        Err(djogi::Error::NotFound) => axum::http::StatusCode::NOT_FOUND.into_response(),
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub async fn posts_list(
    State(pool): State<PgPool>,
) -> impl IntoResponse {
    match Post::objects()
        .filter(|f| f.published.eq(true))
        .order_by(|f| f.published_at.desc())
        .limit(20)
        .fetch_all(&pool).await
    {
        Ok(posts) => Json(posts).into_response(),
        Err(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub fn posts_router() -> axum::Router<PgPool> {
    use axum::routing::get;
    axum::Router::new()
        .route("/posts", get(posts_list))
        .route("/posts/:id", get(post_detail))
}
```

---

## 6. First Test

Djogi uses `#[sqlx::test]` for integration tests. Each test gets an isolated temporary database — no shared state between tests, no teardown ceremony. The pool is automatically created and passed in by the test macro.

```rust
// tests/integration/posts.rs

use djogi::prelude::*;
use sqlx::PgPool;

// Create the table inline in the test. In a full project, use sqlx::test
// with migrations applied automatically via the `fixtures` or `migrator` attributes.
#[sqlx::test]
async fn create_and_fetch_post(pool: PgPool) {
    // Install HeeRanjId schema (needed for generate_id() column default)
    heeranjid_sqlx::install_schema(&pool).await.unwrap();
    heeranjid_sqlx::seed_default_node(&pool).await.unwrap();

    // Create the table matching the model definition
    sqlx::query(
        "CREATE TABLE posts (
            id             BIGINT PRIMARY KEY DEFAULT generate_id(),
            title          TEXT NOT NULL,
            slug           TEXT NOT NULL,
            body           TEXT NOT NULL,
            published      BOOLEAN NOT NULL,
            view_count     INTEGER NOT NULL,
            rating         DOUBLE PRECISION,
            published_at   TIMESTAMPTZ,
            created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
        )"
    )
    .execute(&pool)
    .await
    .unwrap();

    // Create a post
    let post = Post::create(&pool, Post {
        title: "Test Post".into(),
        slug: "test-post".into(),
        body: "Test body".into(),
        published: false,
        view_count: 0,
        rating: None,
        published_at: None,
        ..Default::default()
    })
    .await
    .unwrap();

    // id is populated from RETURNING
    assert!(post.id.as_i64() > 0);
    assert_eq!(post.title, "Test Post");
    assert!(!post.published);

    // Fetch by PK
    let fetched = Post::get(&pool, post.id).await.unwrap();
    assert_eq!(fetched.slug, "test-post");
}

#[sqlx::test]
async fn filter_returns_only_published(pool: PgPool) {
    heeranjid_sqlx::install_schema(&pool).await.unwrap();
    heeranjid_sqlx::seed_default_node(&pool).await.unwrap();

    sqlx::query(
        "CREATE TABLE posts (
            id BIGINT PRIMARY KEY DEFAULT generate_id(),
            title TEXT NOT NULL, slug TEXT NOT NULL, body TEXT NOT NULL,
            published BOOLEAN NOT NULL, view_count INTEGER NOT NULL,
            rating DOUBLE PRECISION, published_at TIMESTAMPTZ,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )"
    )
    .execute(&pool)
    .await
    .unwrap();

    // Seed two posts
    Post::create(&pool, Post {
        title: "Draft".into(), slug: "draft".into(), body: "".into(),
        published: false, view_count: 0, ..Default::default()
    }).await.unwrap();

    Post::create(&pool, Post {
        title: "Live".into(), slug: "live".into(), body: "".into(),
        published: true, view_count: 10, ..Default::default()
    }).await.unwrap();

    let published = Post::objects()
        .filter(|f| f.published.eq(true))
        .fetch_all(&pool)
        .await
        .unwrap();

    assert_eq!(published.len(), 1);
    assert_eq!(published[0].title, "Live");
}
```

> **Note:** In a project with a proper migrations setup, use `#[sqlx::test(migrator = "djogi::MIGRATOR")]` or `#[sqlx::test(migrations = "migrations/")]` to apply your migration files automatically instead of writing CREATE TABLE inline. The inline approach is shown here because it requires no additional setup for a first test.
