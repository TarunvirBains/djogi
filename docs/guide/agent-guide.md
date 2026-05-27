> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Agent Guide

This guide is written for AI coding agents (Claude, GPT, Cursor, etc.)
working in a Djogi codebase. Read it at the start of a session before
touching any model, query, or test code.

Djogi is a Model-first framework for Rust on Postgres. You define Rust structs;
the `#[model]` proc macro derives ORM methods, `FromPgRow` deserialization,
and inventory registration. Your job is to work within that derivation chain
— not around it.

> The Rhai shell and admin console (Maahi) are not available.
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

- `id: HeerIdRecencyBiased` — `BIGINT PRIMARY KEY DEFAULT heerid_next_desc()`, populated via `RETURNING` after INSERT
- `created_at: time::OffsetDateTime` — `TIMESTAMPTZ NOT NULL DEFAULT now()`, set by DB on INSERT
- `updated_at: time::OffsetDateTime` — `TIMESTAMPTZ NOT NULL DEFAULT now()`, updated by Djogi on every `save()`

These three fields are real struct fields after expansion. You must use
`..Default::default()` when constructing a value for `create()`.

**Attribute reference:**

| Attribute | Example | Effect |
|---|---|---|
| `table` | `table = "posts"` | Sets the Postgres table name (required) |
| `pk` | `pk = Serial` | Use `SERIAL` PK (default is `HeerIdRecencyBiased` / BIGINT descending) |
| `pk` | `pk = RanjId` | Use `UUID` PK via `ranjid_next()` |
| `pk` | `pk = HeerId` | Use ascending BIGINT HeerId (historical default) |
| `no_default` | `no_default` | Suppress generated `Default` impl (needed when fields lack `Default`) |
| `rationale` | `rationale = "..."` | Documents behavioral constraints — read before writing code |

For the full attribute list, see [the models guide](./models.md).

**What methods are available after `#[model]`:**

| Method | Signature | Notes |
|---|---|---|
| `Post::create(&mut ctx, value)` | `async -> Result<Post>` | INSERT + RETURNING; framework fields populated |
| `Post::get(&mut ctx, id)` | `async -> Result<Post>` | Fetch by PK; returns `Err(DjogiError::NotFound)` if missing |
| `post.save(&mut ctx)` | `async -> Result<()>` | Full-row UPDATE; `updated_at` refreshed |
| `post.delete(&mut ctx)` | `async -> Result<()>` | DELETE; consumes the instance |
| `post.refresh_from_db(&mut ctx)` | `async -> Result<Post>` | Returns fresh copy from DB |
| `Post::create_with_id(&mut ctx, id, value)` | `async -> Result<Post>` | Only for explicit `pk = HeerId`; INSERT ... ON CONFLICT DO NOTHING for pre-generated IDs |
| `Post::descriptor()` | `-> &'static ModelDescriptor` | For inventory registration — do not call manually |

All methods take `&mut DjogiContext` — construct one with
`DjogiContext::from_pool(pool)` for pool-backed work, or wrap a call
site in `atomic(ctx, |tx| Box::pin(async move { ... })).await?` (the
free function re-exported from `djogi::prelude`) to run inside a
transaction with savepoint nesting and on-commit callback dispatch.
The context pattern-matches on pool-vs-transaction at each
`tokio-postgres` boundary, so the same call site works for either
mode.

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
let mut ctx = DjogiContext::from_pool(pool.clone());
Post::create(&mut ctx, Post {
    title: "My Post".into(),
    body: "Content".into(),
    published: false,
    view_count: 0,
    ..Default::default()   // fills id, created_at, updated_at with zero values
}).await?;               // framework replaces them before INSERT
```

```rust
// WRONG — will not compile; id, created_at, updated_at are missing
Post::create(&mut ctx, Post {
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

### Rule 3: Use `raw_*` escape hatches for queries the Model trait and QuerySet don't cover

The `Model` trait methods cover single-row CRUD (`get`, `create`, `save`,
`delete`). `Model::objects()` returns a `QuerySet<T>` that covers filters,
ordering, pagination, distinct, bulk update, and bulk delete — see the
[queries guide](./queries.md). For anything beyond that surface
(recursive CTEs, set-returning functions, bespoke JOINs), reach for the
raw escape hatches on `DjogiContext`. These methods live on the sealed
`RawAccessExt` extension trait and are unreachable from `DjogiContext`
without the bypass attribute, which is djogi's `unsafe`-equivalent — see
the [Raw SQL escape hatches spec](../spec/raw-sql-escape-hatches.md):

```rust
use djogi::prelude::*;

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): recursive CTE / bespoke JOIN not exposed by QuerySet.
async fn raw_examples(ctx: &mut DjogiContext, post_id: HeerIdRecencyBiased) -> djogi::Result<()> {
    // raw_query — Vec<T> where T: FromPgRow. FromPgRow decoding is
    // positional, so the SELECT list must match Post's column order
    // exactly: the three injected fields (id, created_at, updated_at)
    // followed by the developer-owned fields (title, body, published,
    // view_count). Missing or reordered columns produce a runtime decode
    // error, not a compile error.
    let _posts: Vec<Post> = ctx.raw_query(
        "SELECT id, created_at, updated_at, title, body, published, view_count
         FROM posts WHERE published = $1",
        &[&true],
    ).await?;

    // raw_scalar — single scalar
    let _count: i64 = ctx.raw_scalar(
        "SELECT COUNT(*) FROM posts",
        &[],
    ).await?;

    // raw_execute — no return value (returns rows-affected as u64)
    let _updated = ctx.raw_execute(
        "UPDATE posts SET view_count = view_count + $1 WHERE id = $2",
        &[&1i32, &post_id],
    ).await?;

    Ok(())
}
```

The `#[djogi::deliberately_bypass_convention_with_raw_sql]` attribute is
mandatory — it brings the sealed `RawAccessExt` trait into scope for the
decorated item. Without it, `ctx.raw_*` does not resolve. The adjacent
`// JUSTIFICATION (djogi#<n>): ...` comment names the typed-surface gap
the bypass is filling and is enforced under `tests/` by
`cargo xtask check-justifications`. All three methods take
`&mut DjogiContext`; the same call site works against a pool-backed
context or a transaction-backed one.

### Rule 4: Use transactions explicitly

Wrap multi-step operations in `djogi::transaction::atomic` (re-exported as
`atomic` through the prelude). The closure receives a transaction-backed
`DjogiContext`; commit happens on `Ok`, rollback on `Err`:

```rust
use djogi::prelude::*;

async fn create_post_with_tag(
    ctx: &mut DjogiContext,
    title: String,
    body: String,
) -> djogi::Result<Post> {
    atomic(ctx, |tx| Box::pin(async move {
        let post = Post::create(tx, Post {
            title,
            body,
            ..Default::default()
        }).await?;

        // The tag-write goes through the typed surface — Tag is a
        // `#[model]` struct with a ForeignKey<Post> field.
        Tag::create(tx, Tag {
            post_id: ForeignKey::new(post.id),
            name: "rust".into(),
            ..Default::default()
        }).await?;

        Ok(post)
    })).await
}
```

If either `create()` returns `Err`, the surrounding `atomic` rolls the
transaction back and neither row is persisted. Nested `atomic` calls
push savepoints rather than opening a fresh transaction, so library
helpers can compose without coordinating with their callers.

For raw SQL inside a transaction, the bypass attribute brings the
`raw_*` extension methods into scope on the transaction-backed `tx`
context — see Rule 3 above for the typed-surface check that should run
first, and the [Raw SQL escape hatches spec](../spec/raw-sql-escape-hatches.md)
for the full contract.

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

Default PK is `HeerIdRecencyBiased` (64-bit BIGINT via `heerid_next_desc()`,
reverse-chronological sort order). Use
`pk = HeerId` for ascending BIGINT, `pk = RanjId` for UUIDv8 PKs,
or `pk = Serial` for small reference tables (lookup codes, status
types) where a simple autoincrement is appropriate.

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

**Step 4: Materialise the table from the descriptor — do not hand-write DDL.**

Djogi is descriptor-driven: the `#[model]` macro emits a `ModelDescriptor`
that the migration system and test harness project into SQL. You do not
write `CREATE TABLE` by hand.

- In **production code**, change the struct, rebuild (`cargo build` emits a
  drift warning), then run `djogi migrations compose --name
  add_subscriptions` to generate a reviewable `V<ts>__add_subscriptions.sdjql`
  pair under `migrations/<database>/<app>/`. Library callers apply via
  `djogi::migrate::apply_plan`; see [the migrations guide](./migrations.md).
- In **tests**, list the model in `sync_models = [...]` on the
  `#[djogi::djogi_test]` attribute (Step 5 below) and the harness
  materialises it into the per-test database through the same projection
  pipeline the production runner uses.

Either path produces the same shape — `id BIGINT PRIMARY KEY DEFAULT
heerid_next_desc()`, `created_at TIMESTAMPTZ NOT NULL DEFAULT now()`,
`updated_at TIMESTAMPTZ NOT NULL DEFAULT now()`, plus the developer-owned
columns — projected from the descriptor, not hand-written.

**Step 5: Write your CRUD code and a test.**

```rust
#[djogi::djogi_test(sync_models = [Subscription])]
async fn create_subscription(mut ctx: DjogiContext) {
    // No setup helper — the harness projects the Subscription descriptor
    // into the per-test database before this body runs.
    let sub = Subscription::create(&mut ctx, Subscription {
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

Adding a field is safe — change the struct and let the descriptor-driven
migration system emit the column:

```rust
pub struct Subscription {
    pub plan_name: String,
    pub status: String,
    pub monthly_price_cents: i64,
    pub active: bool,
    pub notes: Option<String>,   // new nullable field
}
```

`cargo build` re-runs the proc macro, updates `target/djogi_models.json`,
and `build.rs` emits a `cargo:warning=` drift line. Run
`djogi migrations compose --name add_subscription_notes` to write
`V<ts>__add_subscription_notes.sdjql` + `.down.sdjql` into the appropriate
`migrations/<database>/<app>/` bucket — review the SQL in your PR, then
apply via the library API (`djogi::migrate::apply_plan`). Use
`djogi migrations attune` only for migration-history ledger/disk
reconciliation; it does not execute migration SQL. See
[the migrations guide](./migrations.md) for the full compose/status/attune
contract; the CLI dispatchers for `apply` / `rollback` / `fake` /
`baseline` / `verify` / `repair` are not available; library
callers use the public `djogi::migrate` entry points directly.

In tests, just add the field to the struct — the next `#[djogi::djogi_test(
sync_models = [Subscription])]` run projects the updated descriptor into
its throwaway database.

**Renaming a field:** annotate the renamed field with
`#[field(renamed_from = "old_name")]` so the differ emits a
`RENAME COLUMN` instead of `DROP + ADD`. Without the annotation the
descriptor diff is structurally indistinguishable from a drop-and-add.

---

## 6. Running Integration Tests

```bash
# Run integration tests
cargo test -p djogi --test phase1_model -- --test-threads=1

# Run a specific test
cargo test -p djogi --test phase1_model create_returns_full_row -- --test-threads=1
```

`--test-threads=1` is the safe default when tests share a Postgres
instance. Each `#[djogi::djogi_test]` gets its own throwaway
database, but tests that touch session-level state
(`set_tenant`, node-id config, `SET LOCAL`) should keep the
serialized flag.

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
  execute it against a `&mut DjogiContext`. A queryset dropped without a
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
  `Post::objects().update(|f| f.published().set(false)).execute(&mut ctx)`
  updates every row in the table. This is intentional, not a safety
  net; wrap in a filter before execution or reach for a transaction if
  you need a rollback path.

- **Empty-assignment short-circuit for `update`.**
  `queryset.update(|_| vec![]).execute(&mut ctx)` returns `Ok(0)` without
  issuing SQL — an `UPDATE ... SET` with no assignments would otherwise
  be a Postgres syntax error. Same for the `.none().update(...)` path.

- **Mutation guard — `update(...).execute(...)`, `execute_returning_pairs(...)`, `delete(...)`, and `delete_returning(...)` reject queryset state that is only meaningful for reads.** The rejected public states are `limit`, `offset`, `distinct`, row locks, explicit `order_by`, `prefetch`, and `select_related` — each surfaces as `DjogiError::Validation` before SQL is issued. The guard runs before the `none()` and empty-assignment short-circuits, so `.none().limit(1).delete()` is rejected (not `Ok(0)`). Explicit `.order_by(...)` is rejected; model-default ordering is not.

- **`updated_at = now()` is stamped on every bulk update.** The SQL
  emitter always appends `updated_at = now()` to the SET list, even
  when the caller's closure omits it. Parity with single-row `save()`.
  Callers who need to preserve `updated_at` across a bulk write drop to
  the `raw_execute` escape hatch (under the bypass attribute — see
  the [Raw SQL escape hatches spec](../spec/raw-sql-escape-hatches.md)).

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
post.delete(&mut ctx).await?;
// use id here — post is moved
```

### Mismatching Rust type and SQL column type

If `FromPgRow` deserialization fails at runtime with a type mismatch,
the Rust type and the Postgres column type are out of sync. Check the
type mapping table in Rule 5 above.

### Using `chrono` types

Djogi uses the `time` crate. `time::OffsetDateTime` for timestamps,
`time::Date` for dates. Using `chrono::DateTime` in a model field will
fail at compile time because `postgres-types` does not implement
`ToSql` / `FromSql` for `chrono` types under Djogi's feature set.

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
| Build a context | `let mut ctx = DjogiContext::from_pool(pool.clone());` |
| Create a record | `Model::create(&mut ctx, Model { ..., ..Default::default() }).await?` |
| Fetch by PK | `Model::get(&mut ctx, id).await?` |
| Update a field | `instance.field = value; instance.save(&mut ctx).await?` |
| Delete | `instance.delete(&mut ctx).await?` (consumes instance) |
| Refresh stale instance | `instance.refresh_from_db(&mut ctx).await?` |
| Pre-generated ID insert | Explicit `pk = HeerId` models only: `Model::create_with_id(&mut ctx, id, Model { ... }).await?` |
| Filter query | `Model::objects().filter(\|f\| f.col().eq(v)).fetch_all(&mut ctx).await?` |
| Count | `Model::objects().filter(\|f\| ...).count(&mut ctx).await?` |
| Bulk update | `Model::objects().filter(\|f\| ...).update(\|f\| f.col().set(v)).execute(&mut ctx).await?` |
| Bulk delete | `Model::objects().filter(\|f\| ...).delete(&mut ctx).await?` |
| Raw query (beyond QuerySet) | `ctx.raw_query::<T>("SELECT ...", &[&val]).await?` (under `#[djogi::deliberately_bypass_convention_with_raw_sql]`) |
| Raw execute | `ctx.raw_execute("UPDATE ...", &[&val]).await?` (under `#[djogi::deliberately_bypass_convention_with_raw_sql]`) |
| Transactional ops | `atomic(&mut ctx, \|tx\| Box::pin(async move { ... })).await?` — re-exported from `djogi::prelude`. Commits on `Ok`, rolls back on `Err`. |
| Iterate all models | `for desc in inventory::iter::<djogi::ModelDescriptor> { ... }` |
| Check trait contract | Read `djogi/src/model.rs` |
| Check field-type mapping | Read `djogi-macros/src/model/attrs.rs::rust_type_to_sql` |
| Run integration tests | `cargo test -p djogi --test phase1_model -- --test-threads=1` |
