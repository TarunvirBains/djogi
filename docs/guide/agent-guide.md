> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Agent Guide

This guide is written for AI coding agents (Claude, GPT, Cursor, etc.)
working in a Djogi codebase. Read it at the start of a session before
touching any model, query, or test code.

Djogi is a Model-first ORM for Rust on Postgres. You define Rust structs;
the `#[model]` proc macro derives ORM methods, `FromRow` deserialization,
and inventory registration. Your job is to work within that derivation chain
— not around it.

> **Current scope:** Phase 1 (models + CRUD + descriptor) and Phase 2
> (`QuerySet<T>` + filters + bulk update/delete) ship. The `cargo djogi`
> CLI, `cargo djogi migrate`, the Rhai shell, relations (FK / M2M),
> RLS / tenant isolation, and the expression layer (Phase 4+) do not.
> This guide covers what actually ships today. Planned features are
> documented in [the roadmap](../roadmap/index.md).

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

### Rule 3: Use `djogi::raw::*` for queries the Model trait and QuerySet don't cover

The `Model` trait methods cover single-row CRUD (`get`, `create`, `save`,
`delete`). `Model::objects()` returns a `QuerySet<T>` that covers filters,
ordering, pagination, distinct, bulk update, and bulk delete — see the
[queries guide](./queries.md). For anything beyond that surface (JOINs,
CTEs, window functions, `col = col + 1`-style expression UPDATEs), use
`djogi::raw::*`:

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

## 7. QuerySet invariants

`Model::objects()` returns a lazy `QuerySet<T>`. The invariants below are
load-bearing — violating them either fails to compile or produces quiet
mis-behaviour the type system cannot catch. Read them before writing any
query code.

- **`Model::objects()` never runs a query.** Construction is free. Only
  the terminal methods (`fetch_all`, `fetch_one`, `first`, `count`,
  `exists`, `update(...).execute(...)`, `delete(...)`) emit SQL and
  execute it against a `sqlx::Executor`. A queryset dropped without a
  terminal silently does nothing; the `#[must_use]` bound on every
  builder method surfaces the dropped-chain case as a lint warning.

- **`fetch_one` enforces exactly-one.** Zero rows → `DjogiError::NotFound`;
  two or more rows → `DjogiError::MultipleObjects` (via an internal
  `LIMIT 2` probe, so the `count_seen` field on the error is the
  sentinel value `2`, not the true matching-row count). When zero-or-one
  is an acceptable outcome, use `first(...)` — it returns
  `Result<Option<T>, DjogiError>` and stops scanning at the first row.

- **`.none()` short-circuits every terminal without touching the DB.**
  Identity results per terminal: `fetch_all` → `Ok(vec![])`, `fetch_one`
  → `Err(DjogiError::NotFound { .. })`, `first` → `Ok(None)`, `count` →
  `Ok(0)`, `exists` → `Ok(false)`, `update(...).execute(...)` → `Ok(0)`,
  `delete(...)` → `Ok(0)`. Any filters / ordering / limits chained
  before `.none()` are discarded; the returned queryset is a fresh
  empty-flagged `QuerySet::new()`.

- **Bulk `update` / `delete` accept "no filter" as "match every row".**
  `Post::objects().update(|f| f.published().set(false)).execute(&pool)`
  updates every row in the table. This is intentional, not a safety
  net; wrap in a filter before execution or reach for a transaction if
  you need a rollback path.

- **Empty-assignment short-circuit for `update`.**
  `queryset.update(|_| vec![]).execute(&pool)` returns `Ok(0)` without
  issuing SQL — an `UPDATE ... SET` with no assignments would otherwise
  be a Postgres syntax error. Same for the `.none().update(...)` path.

- **`updated_at = now()` is stamped on every bulk update.** The SQL
  emitter always appends `updated_at = now()` to the SET list, even
  when the caller's closure omits it. Parity with single-row `save()`.
  Callers who need to preserve `updated_at` across a bulk write drop to
  `djogi::raw::execute`.

- **`FieldRef<M, V>` is `Copy + 'static`.** Free to pass around, bind to
  a local, use twice in one closure. The two phantom markers
  (`PhantomData<fn() -> M>`, `PhantomData<fn() -> V>`) carry no runtime
  state; the whole struct is a `&'static str` column name plus two
  zero-sized tags.

- **`UpdateAssignment` is constructor-locked.** `FieldRef::set(value)`
  is the only public path to an `UpdateAssignment`. The struct's fields
  are `pub(crate)` so downstream crates cannot hand-craft assignments
  with arbitrary column strings or mismatched value shapes — the SQL
  emitter's `unreachable!()` branches on `List`/`Pair`/`Null` values are
  genuinely unreachable from safe code.

- **`{Model}Filter` is emitted alongside `{Model}Fields` by the
  `#[model(table = "...")]` attribute macro.** Same module, same
  visibility. Use it
  (`filter_struct(PostFilter::new().published(Lookup::Eq(true)))`) when
  you cannot write a `|f|` closure at compile time — shell bindings,
  admin UIs, dynamic assemblers. Row-set output is identical to the
  closure form; an integration test asserts parity.

- **`order_by` stacks across calls.** Successive `.order_by(...)` calls
  **append** to the ordering list (Django semantics), they do not
  replace. Library code can safely add a stable tiebreaker without
  clobbering the caller's primary sort key.

- **`FieldRef::in_list(vec![])` renders as SQL `FALSE`**; `not_in_list(vec![])`
  renders as SQL `TRUE`. Avoids the `col IN ()` syntax error and matches
  the documented contract.

- **`contains` / `starts_with` / `ends_with` escape LIKE wildcards.**
  User input containing `%`, `_`, or `\` is escaped before the `%`
  wrapping — `f.title().contains("50%")` matches the literal two-character
  sequence.

---

## 8. Common Mistakes

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
| Filter query | `Model::objects().filter(\|f\| f.col().eq(v)).fetch_all(&pool).await?` |
| Count | `Model::objects().filter(\|f\| ...).count(&pool).await?` |
| Bulk update | `Model::objects().filter(\|f\| ...).update(\|f\| f.col().set(v)).execute(&pool).await?` |
| Bulk delete | `Model::objects().filter(\|f\| ...).delete(&pool).await?` |
| Raw query (beyond QuerySet) | `djogi::raw::query_as(&pool, "SELECT ...", \|q\| q.bind(val)).await?` |
| Raw execute | `djogi::raw::execute(&pool, "UPDATE ...", \|q\| q.bind(val)).await?` |
| Transactional ops | `pool.begin().await?` → pass `&mut *tx` to methods → `tx.commit().await?` |
| Iterate all models | `for desc in inventory::iter::<djogi::ModelDescriptor> { ... }` |
| Check trait contract | Read `djogi/src/model.rs` |
| Check field-type mapping | Read `djogi-macros/src/model/attrs.rs::rust_type_to_sql` |
| Run integration tests | `cargo test -p djogi --test phase1_model -- --test-threads=1` |
