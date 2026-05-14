> [Back to roadmap index](./index.md) | [Shipped guides](../guide/index.md)

# Querying

> **Status: SHIPPED.** This document was the design target for the
> query layer; the API is now live across Phase 2 (`QuerySet<T>` + filters
> + bulk update/delete), Phase 4 (expression IR, annotations, aggregates),
> and Phase 6.5 (grouped aggregation type-state). The authoritative
> current API lives in [`docs/guide/queries.md`](../guide/queries.md),
> [`docs/guide/expressions.md`](../guide/expressions.md), and
> [`docs/guide/query-aggregation.md`](../guide/query-aggregation.md).
> This roadmap document is preserved as design history — the snippets
> here may not match the shipped surface byte-for-byte.

Djogi's query layer is built around `QuerySet<T>` — a lazy, composable builder that accumulates filters, orderings, and options without touching the database. Nothing executes until you call a terminal method.

Under the hood, `QuerySet<T>` compiles its `Condition` tree into SQL via Djogi's own `ConditionBuilder`, a thin wrapper over `sqlx::QueryBuilder<Postgres>`. No third-party query builder is involved — this layer is owned entirely by Djogi, keeping the dependency surface lean.

For queries that exceed the `QuerySet` surface, raw `sqlx::QueryBuilder` is always available as an explicit escape hatch.

---

## Instance Operations

These methods operate on a single known record by primary key or on an already-fetched model instance.

All examples below assume a `DjogiContext` constructed from the application pool:

```rust
use djogi::prelude::*;

let mut ctx = DjogiContext::from_pool(pool.clone());
```

Every method takes `&mut ctx` so Djogi can thread transaction state, hooks, and per-request metadata through the call. To run inside a transaction, shadow `ctx` with `let mut tx_ctx = ctx.begin().await?;` and pass `&mut tx_ctx`.

### `Model::get(ctx, id)`

Fetches a single record by primary key. Returns `Err(djogi::Error::NotFound)` if no row matches.

```rust
let post = Post::get(&mut ctx, id).await?;
```

Generated SQL:
```sql
SELECT id, title, slug, body, published, view_count, rating, published_at, created_at, updated_at
FROM posts
WHERE id = $1
```

### `Model::create(ctx, value)`

Inserts a new row. Framework fields (`id`, `created_at`, `updated_at`) in the input struct are ignored — the framework populates them before the INSERT. The fully populated struct is returned via `RETURNING`.

```rust
let post = Post::create(&mut ctx, Post {
    title: "My Post".into(),
    slug: "my-post".into(),
    body: "Content here...".into(),
    published: false,
    view_count: 0,
    rating: None,
    published_at: None,
    ..Default::default()   // framework fields — replaced before INSERT
}).await?;

println!("{}", post.id);   // populated by RETURNING id
```

Generated SQL:
```sql
INSERT INTO posts (title, slug, body, published, view_count, rating, published_at)
VALUES ($1, $2, $3, $4, $5, $6, $7)
RETURNING id, title, slug, body, published, view_count, rating, published_at, created_at, updated_at
```

### `instance.save(ctx)`

Updates the record in-place. `updated_at` is always refreshed. With dirty tracking disabled (default), a full-row UPDATE is issued. With dirty tracking enabled, only changed fields are included.

```rust
let mut post = Post::get(&mut ctx, id).await?;
post.published = true;
post.save(&mut ctx).await?;
```

Generated SQL (without dirty tracking):
```sql
UPDATE posts
SET title = $1, slug = $2, body = $3, published = $4, view_count = $5,
    rating = $6, published_at = $7, updated_at = $8
WHERE id = $9
```

### `instance.delete(ctx)`

Deletes the record. The method consumes the instance so it cannot be used after deletion.

```rust
let post = Post::get(&mut ctx, id).await?;
post.delete(&mut ctx).await?;
// post is moved — cannot be used here
```

### `instance.save_with_actor(ctx, actor)`

Like `save()`, but writes the actor string to the CRUD audit log entry (when `crud_log = true` is set on the model). Use for attributing changes to a specific user, service, or system.

```rust
post.save_with_actor(&mut ctx, "user:8312847293").await?;
```

---

## QuerySet — Lazy Builder

`QuerySet<T>` is the primary query interface for multi-record operations. It is lazy — no SQL is emitted until a terminal method is called. QuerySets are cheap to clone and compose.

### `Model::objects()`

Returns a new `QuerySet<T>` with no filters, no ordering, and no limit. Nothing executes at this point.

```rust
let qs = Post::objects();  // no DB call yet
```

### Composition and cloning

QuerySets can be cloned and extended without re-executing:

```rust
let published = Post::objects().filter(|f| f.published.eq(true));

// Two independent queries, both starting from the published base
let recent = published.clone()
    .order_by(|f| f.published_at.desc())
    .limit(5)
    .fetch_all(&mut ctx).await?;

let popular = published.clone()
    .order_by(|f| f.view_count.desc())
    .limit(5)
    .fetch_all(&mut ctx).await?;
```

---

## Filtering

### `.filter(|f| ...)` — type-safe closure

The filter closure receives a typed accessor struct for the model's fields. Each field accessor exposes condition methods. Multiple `.filter()` calls are ANDed together.

```rust
// Single condition
Post::objects()
    .filter(|f| f.published.eq(true))
    .fetch_all(&mut ctx).await?;

// Compound conditions using .and() and .or()
Post::objects()
    .filter(|f| f.published.eq(true).and(f.view_count.gte(1000)))
    .fetch_all(&mut ctx).await?;

Post::objects()
    .filter(|f| f.rating.gt(4.0).or(f.view_count.gte(500)))
    .fetch_all(&mut ctx).await?;

// Chained .filter() calls are ANDed
Post::objects()
    .filter(|f| f.published.eq(true))
    .filter(|f| f.view_count.gte(100))   // AND view_count >= 100
    .fetch_all(&mut ctx).await?;
```

### Field condition methods

| Method | SQL equivalent | Notes |
|---|---|---|
| `.eq(val)` | `= $n` | Equality |
| `.neq(val)` | `!= $n` | Inequality |
| `.gt(val)` | `> $n` | Greater than |
| `.gte(val)` | `>= $n` | Greater than or equal |
| `.lt(val)` | `< $n` | Less than |
| `.lte(val)` | `<= $n` | Less than or equal |
| `.in_list(vals)` | `IN ($n, ...)` | Match any value in a slice |
| `.not_in(vals)` | `NOT IN ($n, ...)` | Match no value in a slice |
| `.is_null()` | `IS NULL` | Null check |
| `.is_not_null()` | `IS NOT NULL` | Not null check |
| `.contains(s)` | `ILIKE '%s%'` | Case-insensitive substring match |
| `.starts_with(s)` | `ILIKE 's%'` | Case-insensitive prefix match |
| `.ends_with(s)` | `ILIKE '%s'` | Case-insensitive suffix match |
| `.between(a, b)` | `BETWEEN $n AND $m` | Inclusive range |
| `.and(cond)` | `... AND ...` | Combine two conditions (AND) |
| `.or(cond)` | `... OR ...` | Combine two conditions (OR) |

All values are passed as bound parameters — `$1`, `$2`, etc. — never interpolated into the SQL string. SQL injection through filter values is not possible via the QuerySet API.

### `Option<T>` fields

For nullable columns, `is_null()` and `is_not_null()` are the typed options. `eq(None)` is also accepted and compiles to `IS NULL`:

```rust
Post::objects()
    .filter(|f| f.published_at.is_null())     // published_at IS NULL
    .fetch_all(&mut ctx).await?;

Post::objects()
    .filter(|f| f.rating.is_not_null())       // rating IS NOT NULL
    .filter(|f| f.rating.gte(4.5))
    .fetch_all(&mut ctx).await?;
```

### JSONB subfield filters

For `Jsonb<T>` fields, the proc macro generates typed filter accessors for all known fields using Postgres's JSONB path operators:

```rust
// Filter on a known root-level JSONB field
Vehicle::objects()
    .filter(|f| f.engine.horsepower.gte(300))
    // WHERE (engine->>'horsepower')::integer >= 300
    .fetch_all(&mut ctx).await?;

// Filter on a known nested JSONB field
Vehicle::objects()
    .filter(|f| f.engine.turbo.boost_psi.gte(15.0))
    // WHERE (engine->'turbo'->>'boost_psi')::float >= 15.0
    .fetch_all(&mut ctx).await?;
```

---

## Programmatic Filter API

When filter closures are not available — in the shell, in serialized query state, or when building filters dynamically from runtime input — use `ModelFilter`:

```rust
use djogi::prelude::*;

let filter = PostFilter::new()
    .published(Eq(true))
    .view_count(Gte(100))
    .title(Contains("rust".into()));

let posts = Post::objects()
    .filter_struct(filter)
    .fetch_all(&mut ctx).await?;
```

`ModelFilter` is serializable — it can be stored, transmitted, and reconstructed. The shell uses this API because Rhai closures cannot capture Rust types.

Available operators for `ModelFilter`:

| Operator | Equivalent closure |
|---|---|
| `Eq(val)` | `.eq(val)` |
| `Neq(val)` | `.neq(val)` |
| `Gt(val)` | `.gt(val)` |
| `Gte(val)` | `.gte(val)` |
| `Lt(val)` | `.lt(val)` |
| `Lte(val)` | `.lte(val)` |
| `InList(vals)` | `.in_list(vals)` |
| `IsNull` | `.is_null()` |
| `IsNotNull` | `.is_not_null()` |
| `Contains(s)` | `.contains(s)` |
| `StartsWith(s)` | `.starts_with(s)` |
| `Between(a, b)` | `.between(a, b)` |

---

## Ordering

### `.order_by(|f| f.field.asc())` / `.order_by(|f| f.field.desc())`

```rust
Post::objects()
    .filter(|f| f.published.eq(true))
    .order_by(|f| f.published_at.desc())
    .fetch_all(&mut ctx).await?;
```

Multiple orderings are applied in order:

```rust
Post::objects()
    .order_by(|f| f.published_at.desc())   // primary: most recent first
    .order_by(|f| f.title.asc())           // secondary: alphabetical within same timestamp
    .fetch_all(&mut ctx).await?;
```

---

## Pagination

### `.limit(n)` and `.offset(n)`

```rust
let page_size = 20usize;
let page = 3usize;

let posts = Post::objects()
    .filter(|f| f.published.eq(true))
    .order_by(|f| f.published_at.desc())
    .limit(page_size)
    .offset(page_size * (page - 1))
    .fetch_all(&mut ctx).await?;
```

> **Warning:** Offset pagination degrades with large offsets — Postgres must scan all skipped rows. For high-volume tables, use cursor-based pagination with a `WHERE id > $last_id` filter instead.

---

## Eager Loading (Prefetch)

No lazy loading. No surprise queries. All related data is loaded explicitly.

### `.prefetch(ModelRelated::relation())`

Issues one `IN (...)` query per prefetched relation — never N+1. After `fetch_all()`, the related records are resolved in memory and accessible via `.resolved()`.

```rust
use crate::apps::posts::models::{Comment, CommentRelated};

let comments = Comment::objects()
    .filter(|f| f.post_id.eq(post.id))
    .prefetch(CommentRelated::post())
    .prefetch(CommentRelated::author())
    .fetch_all(&mut ctx).await?;

// After prefetch, resolved() is free — no additional query
for comment in &comments {
    let post = comment.post_id.resolved();    // Option<&Post>
    let author = comment.author_id.resolved(); // Option<&User>
    println!("{}: {}", author.map(|u| u.username.as_str()).unwrap_or("unknown"), comment.body);
}
```

Generated SQL (two queries total, not N+1):
```sql
SELECT id, post_id, author_id, body, created_at, updated_at
FROM comments
WHERE post_id = $1;

SELECT id, title, slug, ... FROM posts WHERE id IN ($1);
SELECT id, username, ... FROM users WHERE id IN ($1, $2, ...);
```

### Single FK fetch

For loading a single related record on an already-fetched instance:

```rust
let comment = Comment::get(&mut ctx, id).await?;
let post = comment.post_id.fetch(&mut ctx).await?;   // one additional query
```

---

## Terminal Methods

Terminal methods execute the accumulated query and return results. All require `&mut DjogiContext` and return `Result`.

| Method | Returns | SQL | Notes |
|---|---|---|---|
| `.fetch_all(ctx)` | `Result<Vec<T>>` | `SELECT ... [WHERE] [ORDER] [LIMIT] [OFFSET]` | Returns empty `Vec` if no rows match |
| `.fetch_one(ctx)` | `Result<T>` | `SELECT ... LIMIT 1` | Returns `Err(NotFound)` if no row |
| `.fetch_optional(ctx)` | `Result<Option<T>>` | `SELECT ... LIMIT 1` | Returns `Ok(None)` if no row |
| `.count(ctx)` | `Result<i64>` | `SELECT COUNT(*) FROM ...` | Applies all filters, ignores order/limit/offset |
| `.exists(ctx)` | `Result<bool>` | `SELECT EXISTS(SELECT 1 FROM ...)` | Efficient existence check |
| `.first(ctx)` | `Result<Option<T>>` | `SELECT ... ORDER BY id ASC LIMIT 1` | Earliest by PK |
| `.last(ctx)` | `Result<Option<T>>` | `SELECT ... ORDER BY id DESC LIMIT 1` | Latest by PK |

```rust
// Check before fetching
let count = Post::objects()
    .filter(|f| f.published.eq(true))
    .count(&mut ctx).await?;

println!("{} published posts", count);

// Existence check (more efficient than count for yes/no questions)
let has_published = Post::objects()
    .filter(|f| f.author_id.eq(user.id).and(f.published.eq(true)))
    .exists(&mut ctx).await?;

// Fetch one or get NotFound
let post = Post::objects()
    .filter(|f| f.slug.eq("my-post"))
    .fetch_one(&mut ctx).await?;

// Fetch one or get None
let maybe_post = Post::objects()
    .filter(|f| f.slug.eq("my-post"))
    .fetch_optional(&mut ctx).await?;

if let Some(post) = maybe_post {
    println!("{}", post.title);
}
```

---

## Bulk Operations

### `Model::bulk_create(ctx, records)`

Inserts multiple records in a single statement. Returns the inserted records with framework fields populated.

```rust
let posts = Post::bulk_create(&mut ctx, vec![
    Post { title: "First".into(), slug: "first".into(), ..Default::default() },
    Post { title: "Second".into(), slug: "second".into(), ..Default::default() },
    Post { title: "Third".into(), slug: "third".into(), ..Default::default() },
]).await?;
```

Generated SQL:
```sql
INSERT INTO posts (title, slug, body, published, view_count, rating, published_at)
VALUES ($1, $2, $3, $4, $5, $6, $7), ($8, $9, $10, ...), ...
RETURNING id, title, slug, ...
```

### Bulk insert with pre-allocated IDs

When you need to cross-reference records in a bulk insert (link tables, for example), pre-allocate IDs before any INSERT fires:

```rust
// Allocate 3 IDs in a single heer_node_state update
let ids = HeerId::generate_many(&pool, 3).await?;

let memberships = vec![
    PersonGroup { id: ids[0], person_id: alice.id, group_id: group.id, role: "admin".into(), ..Default::default() },
    PersonGroup { id: ids[1], person_id: bob.id,   group_id: group.id, role: "member".into(), ..Default::default() },
    PersonGroup { id: ids[2], person_id: carol.id, group_id: group.id, role: "member".into(), ..Default::default() },
];

PersonGroup::bulk_create(&mut ctx, memberships).await?;
```

---

## The `_insecurely()` Variants

Every terminal method has an `_insecurely()` variant that bypasses safety guards (RLS enforcement checks, tenant scoping verification). These are documented in detail in the [Security guide](./security.md).

```rust
// Standard — enforces tenant scoping
Post::objects()
    .filter(|f| f.org_id.eq(tenant_id))
    .fetch_all(&mut ctx).await?;

// Bypasses tenant scoping checks — requires rationale
Post::objects()
    .fetch_all_insecurely(&mut ctx).await?;
```

All `_insecurely()` calls are logged to the event log database with a full stack trace.

---

## Raw SQL Escape Hatch

When the `QuerySet` API cannot express the query you need — complex joins, CTEs, window functions, `LATERAL` subqueries — drop down to `sqlx::QueryBuilder` directly.

```rust
use sqlx::QueryBuilder;

let mut qb: QueryBuilder<sqlx::Postgres> = QueryBuilder::new(
    "SELECT p.id, p.title, COUNT(c.id) AS comment_count
     FROM posts p
     LEFT JOIN comments c ON c.post_id = p.id
     WHERE p.published = "
);
qb.push_bind(true);
qb.push(" GROUP BY p.id, p.title ORDER BY comment_count DESC LIMIT ");
qb.push_bind(10i32);

// You control the deserialization target
#[derive(sqlx::FromRow)]
struct PostWithCount {
    id: HeerId,
    title: String,
    comment_count: i64,
}

let results: Vec<PostWithCount> = qb.build_query_as().fetch_all(&pool).await?;
```

Raw SQL queries are always typed — you declare the row struct via `#[derive(sqlx::FromRow)]`. Untyped dynamic rows are supported via `sqlx::Row` but are discouraged in production code.

### `djogi::raw::query(ctx, sql, binds)`

A convenience wrapper for one-off parameterized queries:

```rust
let results: Vec<PostWithCount> = djogi::raw::query(
    &mut ctx,
    "SELECT id, title, COUNT(c.id) AS comment_count \
     FROM posts p LEFT JOIN comments c ON c.post_id = p.id \
     WHERE p.published = $1 GROUP BY p.id LIMIT $2",
    (true, 10i32),
).await?;
```

### `query_insecurely()`

Like `djogi::raw::query()` but bypasses Djogi's safety checks (no tenant isolation enforcement, no RLS validation). Intended for admin tooling, migrations, and development utilities.

> **Warning:** `query_insecurely()` should never appear in request-handling code paths. Every call is logged. If you find yourself reaching for it in a handler, re-examine the data access pattern — the correct approach is almost always to restructure the query rather than bypass safety guards.

---

## Shell Queries

In the Djogi shell (`djogi shell`), all terminal methods are synchronous — no `.await`, no async ceremony. The shell holds a dedicated `tokio` runtime and wraps every call in `block_on` internally. The API is identical to application code, minus `.await?`:

```rhai
// Shell — identical API, no await
let posts = Post::objects()
    .filter_struct(PostFilter::new().published(Eq(true)))
    .order_by_desc("published_at")
    .limit(10)
    .fetch_all();

pp(posts);

// Programmatic filter — closures cannot capture types in Rhai
let results = Post::objects()
    .filter_struct(PostFilter::new().view_count(Gte(500)))
    .fetch_all();

print(results.len());
```

See the [CLI guide](./cli.md) for the full shell reference.
