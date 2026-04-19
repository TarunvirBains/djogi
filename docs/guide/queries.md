> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Queries

`QuerySet<T>` is Djogi's lazy typed query builder. A queryset accumulates
filters, ordering, distinct mode, and pagination without touching the
database; only terminal methods (`fetch_all`, `count`, `update`, …) emit
SQL and execute it against a `&mut DjogiContext`.

This document is a Phase 2 reference. For features still on the roadmap —
expression-backed SET, JOIN-spanning filters, window/aggregate terminals —
see [the querying roadmap](../roadmap/querying.md).

---

## `Model::objects()`

Every `#[model]`-annotated struct gains an `objects()` default method that
returns an empty `QuerySet<T>`:

```rust
use djogi::prelude::*;

let qs: QuerySet<Post> = Post::objects();
// Nothing has hit the database — `QuerySet` is a structural builder.
```

`objects()` is the only constructor you need. `QuerySet::<T>::new()` also
compiles (it is what `objects()` delegates to) but reads less clearly at
the call site.

Every builder method consumes `self` and returns `Self`. Cloning a
queryset is cheap — the condition tree and ordering list are structural
vectors with no owned SQL buffer — so an `if`/`else` branch that reuses
a partially-built queryset is idiomatic:

```rust
let base = Post::objects().filter(|f| f.published().eq(true));
let qs = if show_archive {
    base.clone().order_by(|f| f.view_count().desc())
} else {
    base.clone().order_by(|f| f.created_at().desc()).limit(20)
};
```

---

## Filters

`.filter(|f| ...)` AND-s a closure's return value onto the accumulating
condition tree. The closure receives a default-constructed `T::Fields`
(a ZST) whose methods return typed `FieldRef<T, V>` handles:

```rust
Post::objects()
    .filter(|f| f.published().eq(true))
    .filter(|f| f.view_count().gte(100))
    // published = $1 AND view_count >= $2
```

Each lookup method consumes the `FieldRef` and returns a `Condition` leaf.
`FieldRef<T, V>` is `Copy + 'static` so the same handle can be used twice
in one closure:

```rust
Post::objects().filter(|f| {
    let v = f.view_count();
    v.gte(10).and_with(v.lte(100))
    // view_count >= $1 AND view_count <= $2
});
```

Fluent combinators `and_with` and `or_with` chain conditions
left-to-right; `Condition::and(a, b)` / `Condition::or(a, b)` are the
equivalent prefix-form constructors.

### `.exclude(|f| ...)` — negation

`.exclude(|f| ...)` is the logical inverse of `.filter` — the inner
condition is wrapped in SQL `NOT` and AND-ed onto the tree:

```rust
Post::objects().exclude(|f| f.title().eq("draft".to_string()))
// NOT (title = $1)
```

### Lookup methods on `FieldRef<T, V>`

The typed closure surface exposes the following lookups. Non-string
methods are available on every `FieldRef<T, V>`; string-only methods
require `V = String`.

| Method | SQL | Applies to |
|---|---|---|
| `.eq(v)` | `col = $1` | any `V: IntoFilterValue` |
| `.neq(v)` | `col <> $1` | any |
| `.gt(v)` / `.gte(v)` | `col > $1` / `col >= $1` | any |
| `.lt(v)` / `.lte(v)` | `col < $1` / `col <= $1` | any |
| `.between(a, b)` | `col BETWEEN $1 AND $2` | any |
| `.in_list(it)` | `col IN ($1, $2, …)`; empty → `FALSE` | any; accepts `IntoIterator<Item = V>` |
| `.not_in_list(it)` | `col NOT IN (…)`; empty → `TRUE` | any |
| `.is_null()` / `.is_not_null()` | `col IS [NOT] NULL` | any `V` (column need not be `Option<T>`) |
| `.iexact(v)` | `LOWER(col) = LOWER($1)` | any |
| `.contains(s)` / `.icontains(s)` | `col ILIKE '%…%'` | `V = String` only |
| `.starts_with(s)` / `.istarts_with(s)` | `col ILIKE '…%'` | `V = String` only |
| `.ends_with(s)` / `.iends_with(s)` | `col ILIKE '%…'` | `V = String` only |
| `.regex(s)` | `col ~ $1` (case-sensitive POSIX) | `V = String` only |
| `.iregex(s)` | `col ~* $1` (case-insensitive POSIX) | `V = String` only |

`contains` / `starts_with` / `ends_with` escape `%`, `_`, and `\` in the
user input before wrapping with the appropriate prefix/suffix `%` — a
search for `"50%"` matches the literal sequence, not "50 followed by
anything".

Non-string columns do not resolve the string-only methods — calling
`age.contains(...)` on an `i64` column is a compile error with a localized
"no method named `contains`" message. The type system is the
documentation.

---

## Ordering

`.order_by(|f| ...)` accumulates ordering expressions. Successive calls
**stack** (Django semantics) — they do not replace:

```rust
Post::objects()
    .order_by(|f| f.published().desc())
    .order_by(|f| f.view_count().asc())
    // ORDER BY published DESC, view_count ASC
```

This means library code can add a stable tiebreaker without clobbering
the caller's primary sort key. If you need replace semantics, build the
queryset in one chain rather than calling `order_by` twice.

The closure returns either a single `OrderExpr` (`f.col.asc()` /
`f.col.desc()`) or a `Vec<OrderExpr>` (`vec![f.a.desc(), f.b.asc()]`).

### NULL positioning

`.asc()` / `.desc()` leave NULL positioning at the Postgres default
(NULLS LAST for ASC, NULLS FIRST for DESC). `.nulls_first()` /
`.nulls_last()` force the modifier explicitly:

```rust
Post::objects()
    .order_by(|f| f.view_count().asc().nulls_first())
    // ORDER BY view_count ASC NULLS FIRST
```

---

## Pagination

`.limit(n: u64)` and `.offset(n: u64)` set the SQL `LIMIT` / `OFFSET`.
Both take `u64` so negative values cannot be constructed; the builder
stores them as `Option<i64>` to match Postgres' `BIGINT` bind type and
`debug_assert!`s against `i64::MAX` overflow.

```rust
Post::objects()
    .order_by(|f| f.created_at().desc())
    .limit(20)
    .offset(40)
```

`.limit(n)` and `.offset(n)` replace any prior value — calling them
twice does not stack, only `.order_by` does.

---

## Distinct

`.distinct()` emits plain `SELECT DISTINCT *`:

```rust
Post::objects().distinct().fetch_all(&mut ctx).await?;
// SELECT DISTINCT * FROM posts
```

`.distinct_on(|f| ...)` emits Postgres' `SELECT DISTINCT ON (cols...)`
— keeps the first row per `(cols...)` tuple according to the query's
`ORDER BY`. The closure returns a single `FieldRef` or a tuple of up to
six `FieldRef`s:

```rust
Post::objects()
    .distinct_on(|f| f.author_id())
    .order_by(|f| vec![f.author_id().asc(), f.created_at().desc()])
    // SELECT DISTINCT ON (author_id) * FROM posts
    // ORDER BY author_id ASC, created_at DESC
```

Both forms override any prior `.distinct` / `.distinct_on`.

`.count()` on a `distinct_on` queryset wraps the query in a subquery
(`SELECT COUNT(*) FROM (SELECT DISTINCT ON ...)`) so the count reflects
the deduplicated row set, not the raw one.

---

## Terminal methods

Terminal methods consume the queryset, emit SQL via
`sqlx::QueryBuilder<Postgres>`, and execute against a caller-provided
`&mut DjogiContext`. Per Phase 4 v3 Q1 the context unifies pool and
transaction handling: construct one with `DjogiContext::from_pool(pool)`
for pool-backed use, or pass the context an enclosing transaction scope
hands you.

| Method | Returns | Notes |
|---|---|---|
| `.fetch_all(&mut ctx)` | `Result<Vec<T>, DjogiError>` | Every matching row. Requires `T: FromRow`. |
| `.fetch_one(&mut ctx)` | `Result<T, DjogiError>` | Exactly one — zero rows → `NotFound`; two or more → `MultipleObjects`. Uses `LIMIT 2` to avoid a `COUNT(*)` round trip. |
| `.first(&mut ctx)` | `Result<Option<T>, DjogiError>` | `LIMIT 1`; returns `None` when no row matches. Pair with `.order_by(...)` for a deterministic choice. |
| `.count(&mut ctx)` | `Result<i64, DjogiError>` | `SELECT COUNT(*) …` (or subquery-wrapped when `distinct_on` is set). |
| `.exists(&mut ctx)` | `Result<bool, DjogiError>` | `SELECT EXISTS(SELECT 1 … LIMIT 1)` — stops scanning at the first match. |

### `fetch_one` exact-one contract

`fetch_one` overrides any user-supplied `limit` with `LIMIT 2`. The
two-element case maps to `DjogiError::MultipleObjects` with a sentinel
`count_seen = 2` — the value is "at least 2", not the true matching-row
count (the cap is intentional; callers who need the precise count call
`.count()` separately).

When you want "any row that matches" rather than "the unique row that
matches", use `.first(...)`.

---

## `.none()` — short-circuit

`queryset.none()` returns a structurally empty queryset: every terminal
method short-circuits to the empty result **without issuing any SQL**.
Any filters / ordering / limits already chained are discarded.

```rust
let qs = if user.is_authenticated {
    Post::objects().filter(|f| f.published().eq(true))
} else {
    Post::objects().none()
};
qs.fetch_all(&mut ctx).await?;  // returns `Ok(vec![])` on the `none()` branch
```

Short-circuit identities per terminal:

| Terminal | Short-circuit return |
|---|---|
| `fetch_all` | `Ok(vec![])` |
| `fetch_one` | `Err(DjogiError::NotFound { .. })` |
| `first` | `Ok(None)` |
| `count` | `Ok(0)` |
| `exists` | `Ok(false)` |
| `update(...).execute(...)` | `Ok(0)` |
| `delete(...)` | `Ok(0)` |

---

## Programmatic filters — `{Model}Filter`

The closure API is the preferred user surface, but three callers cannot
write a Rust closure at compile time:

- The Rhai **shell** — no Rust closures.
- The admin UI — filter criteria arrive over HTTP as `(column, op, value)` triples.
- Dynamic assemblers — search/export jobs built from a config file.

The `#[model(table = "...")]` attribute macro emits a `{Model}Filter`
struct alongside `{Model}Fields`. Each setter takes a `Lookup<V>` whose
`V` is pinned to the column's declared Rust type:

```rust
use djogi::prelude::*;

let filter = PostFilter::new()
    .published(Lookup::Eq(true))
    .view_count(Lookup::Gte(50i32))
    .title(Lookup::Contains("rust".to_string()));

let rows = Post::objects()
    .filter_struct(filter)
    .fetch_all(&mut ctx)
    .await?;
```

`Lookup<V>` variants: `Eq`, `Neq`, `Gt`, `Gte`, `Lt`, `Lte`, `In(Vec<V>)`,
`NotIn(Vec<V>)`, `IsNull`, `IsNotNull`, `Contains(String)`,
`StartsWith(String)`, `EndsWith(String)`, `Between(V, V)`,
`Regex(String)`. `Contains` / `StartsWith` / `EndsWith` map to the
case-insensitive `ILIKE` operators, matching the closure API's default.
`Regex` is case-sensitive POSIX (`~`); the closure API's `.iregex`
(case-insensitive `~*`) does not currently have a `Lookup` equivalent.

`Lookup` is `#[non_exhaustive]` — future phases add variants without a
breaking change.

`filter_struct` produces a structurally equivalent condition tree to the
same set of lookups expressed as `.filter(|f| ...)` — an integration
test asserts row-set parity between the two paths.

Empty filters (`PostFilter::new()` with no setters) short-circuit:
`filter_struct(empty)` is a no-op. Single-clause filters unwrap to a
plain leaf rather than a one-element `And` — the SQL emitter renders
`col = $1` without redundant parentheses.

---

## Bulk update and delete

### `update(|f| f.col.set(v)).execute(&mut ctx)`

`.update(...)` builds a pending `UpdateStmt<T>`; the actual `UPDATE`
runs when the caller invokes `.execute(&mut ctx)`. The closure returns a
single `UpdateAssignment` or a `Vec<UpdateAssignment>` built via
`FieldRef::set`:

```rust
let n = Post::objects()
    .filter(|f| f.published().eq(true))
    .update(|f| f.view_count().set(999i32))
    .execute(&mut ctx)
    .await?;
// UPDATE posts SET view_count = $1, updated_at = now() WHERE published = $2
```

`updated_at = now()` is always appended to the SET list — parity with
the single-row `save()` path, which also bumps `updated_at` on every
write. Callers who need to preserve `updated_at` across a bulk update
reach for the raw escape hatch below.

The returned count is `u64` — sqlx's `rows_affected()` passed through
unchanged.

Empty-assignment short-circuit: `filter(...).update(|_| vec![])` returns
`Ok(0)` without issuing SQL. An `UPDATE ... SET` with no assignments is
a Postgres syntax error, so the short-circuit is load-bearing.

Expression-backed SET (`col = col + 1`, `col = NOW()`,
`col = other_col`) is not in Phase 2 — see the [query roadmap][phase-4]
for the Phase 4 expression layer, or drop to raw SQL for the one-off case.

[phase-4]: ../roadmap/querying.md

### `delete(&mut ctx)`

```rust
let n = Post::objects()
    .filter(|f| f.published().eq(false))
    .delete(&mut ctx)
    .await?;
// DELETE FROM posts WHERE published = $1
```

`.delete(&mut ctx)` is a terminal directly on `QuerySet` (no intermediate
pending struct — there's no payload to carry across a split). An
unfiltered queryset deletes every row in the table; "wipe this table"
DDL-style reaches for `TRUNCATE` via `djogi::raw::execute`.

---

## Raw escape hatch

When `QuerySet` can't express the query — CTEs, recursive queries,
window functions, JOINs (Phase 3), arbitrary SQL expressions (Phase 4) —
drop to `sqlx::QueryBuilder<Postgres>` or the `djogi::raw::*` helpers.
See [Models §Rule 3][models-raw] for the raw-query surface.

[models-raw]: ./agent-guide.md#rule-3-use-djogiraw-for-queries-the-model-trait-and-queryset-dont-cover

The raw path sits next to the typed one — a query that starts as
`QuerySet` can pick up a raw tail when a feature isn't shipped yet,
without migrating the entire call site.
