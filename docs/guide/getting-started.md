> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Getting Started

This guide walks through setting up a Djogi project from scratch, defining
your first model, creating the table, and performing CRUD operations — from
a blank directory to a passing integration test.

Djogi targets PostgreSQL 18 and later, exclusively. The framework fields
(`id`, `created_at`, `updated_at`) are injected by the proc macro — you
define only the fields you own.

The data-layer substrate is `tokio-postgres` + `deadpool-postgres` +
`postgres-types`. You do not interact with those crates directly in
typical app code; Djogi exposes a single `DjogiContext` surface that
covers connection pooling, transactions, and the raw-SQL escape hatch.

---

## 1. Workspace Setup

### Prerequisites

- Rust toolchain (stable, 1.95 or later)
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
djogi = "0.1"

tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"

time = { version = "0.3", features = ["serde", "formatting", "parsing"] }
```

> **Note:** Djogi uses the `time` crate for all datetime types — not
> `chrono`. Do not add `chrono` as a dependency.

You do not need to add `tokio-postgres`, `deadpool-postgres`, or
`postgres-types` directly — Djogi re-exports the types you need
(`DjogiPool`, `DjogiContext`, `FromPgRow`) and pulls the underlying
crates as transitive deps.

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
    pub id: HeerIdRecencyBiased,        // BIGINT HeerIdDesc-backed injected PK
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
- `impl FromPgRow for Article` — `tokio-postgres::Row` → `Article` decode
- `Article::descriptor()` — submitted via `inventory` for app registration

---

## 3. Bootstrap the Database

HeeRanjId provides the Postgres functions behind Djogi's default
`HeerIdRecencyBiased` primary key. Adopters do not call the `heeranjid` crate
directly in ordinary Djogi projects: descriptor-driven migration plans include
the required bootstrap, and `#[djogi::djogi_test]` applies the same bootstrap for
each isolated test database. Use `djogi migrations compose` for reviewed
DDL and `sync_models = [...]` in tests.

For production sizing of the connection pool — `max_size`, wait
timeout, per-connection setup hook, raw-client escape hatch — see the
[Connection Pool guide](./pool.md). `DjogiPool::connect(url)` here is
fine for the getting-started flow; production services tune through
`DjogiPool::builder(url)` or `DjogiPool::from_database_config(&cfg.database)`.

---

## 4. Create the Table

Djogi materialises tables from the same `ModelDescriptor` the proc
macro emits, so there is no hand-written DDL to keep in sync with the
struct definition.

**Production / development.** Use the descriptor-driven migration
differ to compose a migration from descriptor drift:

```bash
djogi migrations compose          # generate up/down SQL from descriptor drift
djogi migrations status           # show ledger / snapshot / live-DB state
djogi migrations attune           # reconcile disk / ledger / live DB
```

Apply the composed plan through the library API
(`djogi::migrate::apply_plan` / `rollback_plan` / `fake_apply_plan` /
`baseline_plan`) — the operator-facing `djogi migrations apply` CLI
dispatcher is deferred to a Phase 7 follow-up. See the
[Migrations guide](./migrations.md) for the full pipeline (compose /
apply / attune), library entry points, online-safety classification,
and the ledger.

**Tests.** Annotate the test with `sync_models = [...]` and the
`#[djogi::djogi_test]` harness materialises the listed models into the
per-test ephemeral database — same projection pipeline as the
production runner, no hand-written DDL:

```rust
#[djogi::djogi_test(sync_models = [Article])]
async fn create_articles(mut ctx: DjogiContext) {
    let _article = Article::create(&mut ctx, /* ... */).await.unwrap();
}
```

The column order in the materialised table does not matter for
`FromPgRow` decode — Djogi's canonical SELECT projection is baked at
macro time and the wire shape matches the struct-field order, so
positional decode is sound and fast.

---

## 5. CRUD

With the table created, use the generated `Model` trait methods:

### Create

```rust
use djogi::prelude::*;

async fn create_article(ctx: &mut DjogiContext) -> djogi::Result<Article> {
    let article = Article::create(ctx, Article {
        title: "Getting Started with Djogi".into(),
        slug: "getting-started".into(),
        body: "Djogi is a Model-first framework for Rust...".into(),
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
async fn fetch_article(ctx: &mut DjogiContext, id: HeerIdRecencyBiased) -> djogi::Result<Article> {
    let article = Article::get(ctx, id).await?;
    println!("{}: {}", article.id, article.title);
    Ok(article)
}
```

Returns `Err(DjogiError::NotFound)` when no row matches.

### Update

```rust
async fn publish_article(ctx: &mut DjogiContext, id: HeerIdRecencyBiased) -> djogi::Result<()> {
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
async fn remove_article(ctx: &mut DjogiContext, id: HeerIdRecencyBiased) -> djogi::Result<()> {
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

For queries the `Model` trait and `QuerySet` API don't cover — bespoke
joins, recursive CTEs, set-returning functions — Djogi exposes raw
helpers (`raw_query`, `raw_scalar`, `raw_execute`, `raw_ddl`, …) on
the sealed `djogi::__bypass::RawAccessExt` extension trait. Raw SQL is
treated culturally the way `unsafe` is in Rust: not banned, but always
conscious. The trait is reachable only through the
`#[djogi::deliberately_bypass_convention_with_raw_sql]` attribute,
which brings the extension into scope for the decorated item. Pair the
attribute with a `JUSTIFICATION` comment naming the typed-surface gap
the bypass is filling.

See the [raw SQL escape hatches spec](../spec/raw-sql-escape-hatches.md)
for the full contract.

```rust
use djogi::prelude::*;

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): bespoke ranking query not exposed by QuerySet.
async fn published_articles(ctx: &mut DjogiContext) -> djogi::Result<Vec<Article>> {
    // raw_query — returns Vec<T> for any T: FromPgRow
    let articles: Vec<Article> = ctx.raw_query(
        "SELECT id, created_at, updated_at, title, slug, body, published, view_count
         FROM articles WHERE published = $1",
        &[&true],
    ).await?;

    // raw_scalar — returns a single scalar value
    let _count: i64 = ctx.raw_scalar(
        "SELECT COUNT(*) FROM articles",
        &[],
    ).await?;

    // raw_execute — runs a statement without returning rows; returns rows-affected
    let _updated = ctx.raw_execute(
        "UPDATE articles SET view_count = view_count + 1 WHERE published = $1",
        &[&true],
    ).await?;

    Ok(articles)
}
```

All three methods take `&mut DjogiContext` — the same call site works
against a pool-backed context or a transaction-backed one. Parameters
go in `&[&dyn ToSql]` form using `postgres-types::ToSql` (re-exported as
`djogi::ToSql`).

The bypass attribute is mandatory: without it, `RawAccessExt` is not in
scope and `ctx.raw_*` does not resolve. Inside djogi's own integration
tests under `tests/integration/` the bypass is forbidden by policy
(GH #133) — tests use the typed surface plus `#[djogi_test(sync_models
= [...])]`, and the rare deliberate pin tests under `tests/pin/` carry
`JUSTIFICATION (PIN): exercises raw_<api> itself`.

---

## 7. Transactions

Model CRUD methods take `&mut DjogiContext`. Wrap a call site in
`atomic(ctx, |tx| Box::pin(async move { ... }))` (the free function
re-exported from `djogi::prelude`) to run a closure inside a
transaction with savepoint nesting and on-commit callbacks. The
closure must return `Pin<Box<dyn Future<…>>>` — the `Box::pin(async
move { … })` wrapper is how you spell it:

```rust
use djogi::prelude::*;

async fn transfer_views(ctx: &mut DjogiContext, from_id: HeerIdRecencyBiased, to_id: HeerIdRecencyBiased)
    -> djogi::Result<()>
{
    atomic(ctx, |tx| Box::pin(async move {
        let mut source = Article::get(tx, from_id).await?;
        let mut dest = Article::get(tx, to_id).await?;

        dest.view_count += source.view_count;
        source.view_count = 0;

        source.save(tx).await?;
        dest.save(tx).await?;

        Ok(())
    })).await
}
```

If either `save()` returns an error inside the closure, the transaction
rolls back and neither row is modified. `atomic()` also handles
savepoint nesting (calling `atomic` inside another `atomic` opens a
SAVEPOINT) and dispatches `on_commit` callbacks only after the
outermost transaction commits.

For low-level cases where you need to hand-manage a transaction, use
`DjogiContext::from_connection(conn)` — see `djogi::DjogiContext`
docs for the full surface.

---

## 8. First Test

Use `#[djogi::djogi_test]` for integration tests. Each test gets an
isolated throwaway database, with `install_schema` and
`seed_default_node` applied automatically — no shared state, no teardown
ceremony, no per-test setup helper. Pass `sync_models = [...]` so the
harness materialises the model set into the per-test database through
the same projection pipeline the production runner uses — no
hand-written DDL, no `raw_*` reach.

```rust
// tests/integration/articles.rs
use djogi::prelude::*;

#[djogi::djogi_test(sync_models = [Article])]
async fn create_and_get(mut ctx: DjogiContext) {
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

#[djogi::djogi_test(sync_models = [Article])]
async fn save_updates_fields(mut ctx: DjogiContext) {
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

#[djogi::djogi_test(sync_models = [Article])]
async fn delete_removes_row(mut ctx: DjogiContext) {
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
# Requires DATABASE_URL pointing to a Postgres 18 instance the harness
# can create throwaway databases against.
cargo test --test articles -- --test-threads=1
```

The `--test-threads=1` flag is the safe default while integration tests
share a Postgres instance. The `#[djogi_test]` harness makes parallel
runs safe in principle (each test gets its own DB), but tests that
poke at session-level state — e.g. `set_tenant`, `SET LOCAL`,
node-id config — should keep the serialized flag.

---

## What's Next

- [Models guide](./models.md) — every `#[model(...)]` and `#[field(...)]`
  attribute, including alternate PK types (`serial`, `ranjid`,
  `ranjid_recency_biased`, custom via `djogi::primary_key!`) and the rich
  field types (`Decimal`, `Vec<T>`, `time::Date`, `Jsonb<T>`, `GeoPoint`).
- [Queries guide](./queries.md) — the lazy `QuerySet<T>` API: typed
  filters via `FieldRef`, eager loading via `prefetch` /
  `select_related`, aggregates and annotations, raw-SQL escape hatch.
- [Migrations guide](./migrations.md) — the descriptor-driven differ,
  `djogi migrations compose / status / attune`, online-safety
  classification, and the `djogi live` backfill orchestrator.
- [Agent guide](./agent-guide.md) — if you are an AI coding agent working
  in a Djogi codebase, start here.
