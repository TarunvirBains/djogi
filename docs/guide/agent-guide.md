> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Agent Guide

This guide is written for AI coding agents (Claude, GPT, Cursor, etc.)
working in a Djogi codebase. Read it at the start of a session before
touching any model, query, or test code.

Djogi is a Model-first ORM for Rust on Postgres. You define Rust structs;
the `#[model]` proc macro derives ORM methods, `FromRow` deserialization,
and inventory registration. Your job is to work within that derivation chain
— not around it.

> **Phase 1 scope:** The `cargo djogi` CLI, QuerySet filter closures, RLS /
> tenant isolation, the Rhai shell, and `cargo djogi migrate` are not
> implemented yet. This guide covers what actually ships in Phase 1.
> Planned features are documented in [the roadmap](../roadmap/index.md).

---

## 1. Reading a Model Definition

When you see a model in the codebase, this is what you are looking at:

```rust
#[model(table = "posts")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Post {
    pub title: String,
    pub body: String,
    pub published: bool,
    pub view_count: i32,
}
```

**What this tells you:**

- `table = "posts"` — the Postgres table name is `posts`
- `title: String` — `TEXT NOT NULL` column
- `body: String` — `TEXT NOT NULL` column
- `published: bool` — `BOOLEAN NOT NULL` column
- `view_count: i32` — `INTEGER NOT NULL` column

**What is injected by the macro (not written in the struct):**

- `id: HeerId` — `BIGINT PRIMARY KEY DEFAULT generate_id()`, populated via `RETURNING` after INSERT
- `created_at: time::OffsetDateTime` — `TIMESTAMPTZ NOT NULL DEFAULT now()`, set by DB on INSERT
- `updated_at: time::OffsetDateTime` — `TIMESTAMPTZ NOT NULL DEFAULT now()`, updated by Djogi on every `save()`

These three fields are real struct fields after expansion. You must use
`..Default::default()` when constructing a value for `create()`.

**Phase 1 attribute reference:**

| Attribute | Example | Effect |
|---|---|---|
| `table` | `table = "posts"` | Sets the Postgres table name (required) |
| `pk` | `pk = "serial"` | Use `SERIAL` PK (default is `HeerId` / BIGINT) |
| `pk` | `pk = "ranjid"` | Use `UUID` PK via `generate_ranjid()` |
| `no_default` | `no_default` | Suppress generated `Default` impl (needed when fields lack `Default`) |
| `rationale` | `rationale = "..."` | Documents behavioral constraints — read before writing code |

For the full attribute list, see [the models guide](./models.md).

**What methods are available after `#[model]`:**

| Method | Signature | Notes |
|---|---|---|
| `Post::create(exec, value)` | `async -> Result<Post>` | INSERT + RETURNING; framework fields populated |
| `Post::get(exec, id)` | `async -> Result<Post>` | Fetch by PK; returns `Err(DjogiError::NotFound)` if missing |
| `post.save(exec)` | `async -> Result<()>` | Full-row UPDATE; `updated_at` refreshed |
| `post.delete(exec)` | `async -> Result<()>` | DELETE; consumes the instance |
| `post.refresh_from_db(exec)` | `async -> Result<Post>` | Returns fresh copy from DB |
| `Post::create_with_id(exec, id, value)` | `async -> Result<Post>` | INSERT ... ON CONFLICT DO NOTHING; for pre-generated IDs |
| `Post::descriptor()` | `-> &'static ModelDescriptor` | For inventory registration — do not call manually |

All methods accept any `sqlx::Executor<Database = Postgres>` — pass `pool`
directly or `&mut *tx` for transaction-scoped operations.

---

## 2. Iterating Registered Models

The `#[model]` macro submits a `ModelDescriptor` to `inventory`. To see all
registered models at runtime:

```rust
use djogi::ModelDescriptor;

for desc in inventory::iter::<ModelDescriptor> {
    println!("table: {}, pk: {:?}", desc.table_name, desc.pk_type);
    for field in desc.fields {
        println!("  field: {} ({:?}, nullable: {})", field.name, field.sql_type, field.nullable);
    }
}
```

`inventory::iter::<T>` is a zero-sized type implementing `IntoIterator` —
use it WITHOUT parentheses (not `inventory::iter::<T>()`).

This is how you enumerate the schema at runtime without touching Postgres.
The descriptor for each model is submitted at link time.

---

## 3. Golden Rules

Follow these unconditionally.

### Rule 1: Never write `id`, `created_at`, or `updated_at` by hand

These three fields are injected by the macro. When constructing a value to
pass to `create()`, use `..Default::default()` to fill them:

```rust
// CORRECT
Post::create(&pool, Post {
    title: "My Post".into(),
    body: "Content".into(),
    published: false,
    view_count: 0,
    ..Default::default()   // fills id, created_at, updated_at with zero values
}).await?;               // framework replaces them before INSERT
```

```rust
// WRONG — will not compile; id, created_at, updated_at are missing
Post::create(&pool, Post {
    title: "My Post".into(),
    body: "Content".into(),
    published: false,
    view_count: 0,
}).await?;
```

### Rule 2: Read `rationale` before touching a model

If a model or field has a `#[model(rationale = "...")]` or
`#[field(rationale = "...")]`, read it before writing any code. The
rationale captures behavioral constraints, write patterns, and ownership
rules that the type system cannot encode. Ignoring it produces bugs that are
invisible until production.

### Rule 3: Use `djogi::raw::*` for queries the Model trait doesn't cover

Phase 1 does not have a QuerySet filter API. For anything beyond `get()` and
`create()`, use `djogi::raw::*`:

```rust
// query_as — Vec<T> where T: FromRow
let posts: Vec<Post> = djogi::raw::query_as(
    &pool,
    "SELECT * FROM posts WHERE published = $1",
    |q| q.bind(true),
).await?;

// query_scalar — single scalar
let count: i64 = djogi::raw::query_scalar(
    &pool,
    "SELECT COUNT(*) FROM posts",
    |q| q,
).await?;

// execute — no return value
djogi::raw::execute(
    &pool,
    "UPDATE posts SET view_count = view_count + $1 WHERE id = $2",
    |q| q.bind(1i32).bind(post_id.as_i64()),
).await?;
```

All three accept any `sqlx::Executor` — pass `&mut *tx` to run inside a
transaction.

### Rule 4: Use transactions explicitly

Model methods and `djogi::raw::*` both accept `&mut *tx`. Wrap multi-step
operations in a transaction:

```rust
let mut tx = pool.begin().await?;

let post = Post::create(&mut *tx, Post { ... ..Default::default() }).await?;
djogi::raw::execute(
    &mut *tx,
    "INSERT INTO tags (post_id, name) VALUES ($1, $2)",
    |q| q.bind(post.id.as_i64()).bind("rust"),
).await?;

tx.commit().await?;
```

If either step fails, drop the transaction and neither change is persisted.

### Rule 5: Match field types exactly

Use the type that maps to the correct Postgres column type:

| Rust type | Postgres column |
|---|---|
| `String` | `TEXT NOT NULL` |
| `Option<String>` | `TEXT` (nullable) |
| `bool` | `BOOLEAN NOT NULL` |
| `i32` | `INTEGER NOT NULL` |
| `i64` | `BIGINT NOT NULL` |
| `f64` | `DOUBLE PRECISION NOT NULL` |
| `Decimal` | `NUMERIC NOT NULL` |
| `time::OffsetDateTime` | `TIMESTAMPTZ NOT NULL` |
| `time::Date` | `DATE NOT NULL` |
| `Vec<String>` | `TEXT[] NOT NULL` |
| `Vec<i32>` | `INTEGER[] NOT NULL` |
| `HeerId` | `BIGINT` (used for FK references) |
| `uuid::Uuid` | `UUID` |

Do not use `chrono` types. Do not use `i64` where `HeerId` is needed for a
foreign key reference — HeerId carries type safety.

---

## 4. How to Write a New Model

**Step 1: Identify the table name and PK type.**

Default PK is `HeerId` (64-bit BIGINT via `generate_id()`). Use
`pk = "ranjid"` for UUIDv8 PKs. Use `pk = "serial"` for small reference
tables (lookup codes, status types) where a simple autoincrement is
appropriate.

**Step 2: Write the struct with developer-owned fields only.**

Do not write `id`, `created_at`, or `updated_at`:

```rust
#[model(table = "subscriptions")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub plan_name: String,
    pub status: String,
    pub monthly_price_cents: i64,
    pub active: bool,
}
```

**Step 3: Check the trait contract in `djogi/src/model.rs`.**

The `Model` trait definition lives there. If you are unsure what a method
returns or accepts, read that file directly.

**Step 4: Create the table manually (Phase 1).**

Match each developer field to its SQL type, plus the three injected
framework columns:

```sql
CREATE TABLE subscriptions (
    id                  BIGINT      PRIMARY KEY DEFAULT generate_id(),
    created_at          TIMESTAMPTZ NOT NULL    DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL    DEFAULT now(),
    plan_name           TEXT        NOT NULL,
    status              TEXT        NOT NULL,
    monthly_price_cents BIGINT      NOT NULL,
    active              BOOLEAN     NOT NULL
);
```

**Step 5: Write your CRUD code and a test.**

```rust
#[sqlx::test]
async fn create_subscription(pool: PgPool) {
    // setup: install schema + create table (see Getting Started guide)
    setup_subscriptions(&pool).await;

    let sub = Subscription::create(&pool, Subscription {
        plan_name: "pro".into(),
        status: "active".into(),
        monthly_price_cents: 2900,
        active: true,
        ..Default::default()
    }).await.unwrap();

    assert!(sub.id.as_i64() > 0);
    assert_eq!(sub.plan_name, "pro");
}
```

---

## 5. How to Add a New Field

Adding a field is safe — just add it to the struct and update the table
manually:

```rust
pub struct Subscription {
    pub plan_name: String,
    pub status: String,
    pub monthly_price_cents: i64,
    pub active: bool,
    pub notes: Option<String>,   // new nullable field
}
```

Then add the column to Postgres:

```sql
ALTER TABLE subscriptions ADD COLUMN notes TEXT;
```

In Phase 1 there is no automatic migration differ. Column changes are
manual. The migration system is a Phase 6–8 deliverable — see
[the CLI roadmap](../roadmap/cli.md).

**Renaming a field:** rename the Rust field and update the column. When the
migration system ships, use `#[field(renamed_from = "old_name")]` to tell
the differ to generate `RENAME COLUMN` instead of `DROP + ADD`.

---

## 6. Running Integration Tests

```bash
# Run all Phase 1 integration tests
cargo test -p djogi --test phase1_model -- --test-threads=1

# Run a specific test
cargo test -p djogi --test phase1_model create_returns_full_row -- --test-threads=1
```

`--test-threads=1` is required when tests share a Postgres instance.
Individual `#[sqlx::test]` tests each get an isolated database, but
`ALTER DATABASE` calls across simultaneous tests can conflict.

To validate field type expectations, check
`djogi-macros/src/model/attrs.rs::rust_type_to_sql` — this is the
authoritative mapping from Rust types to SQL types used by the proc macro.

---

## 7. Common Mistakes

### Forgetting `..Default::default()` on `create()`

The injected fields (`id`, `created_at`, `updated_at`) are real struct
fields. Construct with `..Default::default()` — the framework replaces the
zero values before the INSERT fires.

### Calling `save()` on a deleted instance

`delete()` consumes the instance. The Rust compiler will reject uses after
`delete()` at compile time. If you need the ID after deletion, capture it
before calling `delete()`:

```rust
let id = post.id;
post.delete(&pool).await?;
// use id here — post is moved
```

### Mismatching Rust type and SQL column type

If `FromRow` deserialization fails at runtime with a type mismatch, the
Rust type and the Postgres column type are out of sync. Check the type
mapping table in Rule 5 above.

### Using `chrono` types

Djogi uses the `time` crate. `time::OffsetDateTime` for timestamps,
`time::Date` for dates. Using `chrono::DateTime` in a model field will fail
at compile time when sqlx tries to encode/decode it.

### Reading `inventory::iter` with parentheses

`inventory::iter::<ModelDescriptor>` is a ZST that implements
`IntoIterator`. Do not call it: `inventory::iter::<ModelDescriptor>()` is a
compile error. Use it as a value:

```rust
for desc in inventory::iter::<ModelDescriptor> {
    // ...
}
```

---

## Quick Reference

| Task | Correct approach |
|---|---|
| Create a record | `Model::create(&pool, Model { ..., ..Default::default() }).await?` |
| Fetch by PK | `Model::get(&pool, id).await?` |
| Update a field | `instance.field = value; instance.save(&pool).await?` |
| Delete | `instance.delete(&pool).await?` (consumes instance) |
| Refresh stale instance | `instance.refresh_from_db(&pool).await?` |
| Pre-generated ID insert | `Model::create_with_id(&pool, id, Model { ... }).await?` |
| Filter/aggregate query | `djogi::raw::query_as(&pool, "SELECT ...", \|q\| q.bind(val)).await?` |
| Count | `djogi::raw::query_scalar::<i64, _, _>(&pool, "SELECT COUNT(*) ...", \|q\| q).await?` |
| Execute DML | `djogi::raw::execute(&pool, "UPDATE ...", \|q\| q.bind(val)).await?` |
| Transactional ops | `pool.begin().await?` → pass `&mut *tx` to methods → `tx.commit().await?` |
| Iterate all models | `for desc in inventory::iter::<djogi::ModelDescriptor> { ... }` |
| Check trait contract | Read `djogi/src/model.rs` |
| Check field-type mapping | Read `djogi-macros/src/model/attrs.rs::rust_type_to_sql` |
| Run integration tests | `cargo test -p djogi --test phase1_model -- --test-threads=1` |
