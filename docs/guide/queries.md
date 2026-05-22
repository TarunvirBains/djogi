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

### Lookup methods

The typed closure surface exposes the following lookups. The
`{Model}Fields` ZST received by the closure returns
`DjogiField<T, V>` handles; the `DjogiField` API wraps the inner
`FieldRef<T, V>` and exposes the portable (cache-binding eligible)
variants alongside it. Non-string methods are available on any `V`;
string-only methods require `V = String`.

| Method | SQL | Applies to |
|---|---|---|
| `.eq(v)` | `col = $1` | any `V: IntoFilterValue` |
| `.neq(v)` | `col <> $1` | any |
| `.gt(v)` / `.gte(v)` | `col > $1` / `col >= $1` | any |
| `.lt(v)` / `.lte(v)` | `col < $1` / `col <= $1` | any |
| `.between(a, b)` | `col BETWEEN $1 AND $2` | any |
| `.in_(it)` | `col IN ($1, $2, …)`; empty → `FALSE` | any; portable / cache-binding eligible; accepts `IntoIterator<Item = V>` |
| `.not_in(it)` | `col NOT IN (…)`; empty → `TRUE` | any; portable / cache-binding eligible |
| `.in_list(it)` | `col IN ($1, $2, …)`; empty → `FALSE` | any; non-portable (raw `Condition`); accepts `IntoIterator<Item = V>` |
| `.not_in_list(it)` | `col NOT IN (…)`; empty → `TRUE` | any; non-portable (raw `Condition`) |
| `.is_null()` / `.is_not_null()` | `col IS [NOT] NULL` | any `V` (column need not be `Option<T>`) |
| `.iexact(v)` | `LOWER(col) = LOWER($1)` | any |
| `.contains(s)` / `.icontains(s)` | `col ILIKE '%…%'` | `V = String` only |
| `.starts_with(s)` / `.istarts_with(s)` | `col ILIKE '…%'` | `V = String` only |
| `.ends_with(s)` / `.iends_with(s)` | `col ILIKE '%…'` | `V = String` only |
| `.regex(s)` | `col ~ $1` — Postgres POSIX regex operator | `V = String` only |
| `.iregex(s)` | `col ~* $1` — case-insensitive Postgres POSIX regex | `V = String` only |

`contains` / `starts_with` / `ends_with` escape `%`, `_`, and `\` in the
user input before wrapping with the appropriate prefix/suffix `%` — a
search for `"50%"` matches the literal sequence, not "50 followed by
anything".

`regex` and `iregex` are Postgres features, not Rust ones — `s` is a
Postgres POSIX regex pattern (the same syntax accepted by the `~` and
`~*` SQL operators), evaluated entirely server-side. Djogi never
links a Rust regex engine; the `regex` rule in
[`docs/spec/decisions.md`](../spec/decisions.md) carves out the
Postgres-side `~` / `~*` operators because they are SQL operators
exposed through the typed query API. For literal-substring matching,
prefer `contains` — it escapes `%`, `_`, and `\` and is cheaper to
plan. Reach for `regex` only when the predicate genuinely needs
alternation, anchors, or character classes.

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

## Row-level locking

Djogi surfaces Postgres' two row-lock families on every queryset.
Builder methods are last-call-wins and `must_use` — chaining them
without a terminal silently drops the lock.

| Builder | SQL tail | Lock kind | Contention behaviour |
|---|---|---|---|
| `.select_for_update()` | `FOR UPDATE` | exclusive | blocks until released |
| `.nowait()` | `FOR UPDATE NOWAIT` | exclusive | fails fast with `DjogiError::LockConflict` |
| `.skip_locked()` | `FOR UPDATE SKIP LOCKED` | exclusive | silently omits locked rows |
| `.select_for_share()` | `FOR SHARE` | shared | blocks writers, allows peer readers |
| `.for_share_nowait()` | `FOR SHARE NOWAIT` | shared | fails fast with `DjogiError::LockConflict` |
| `.for_share_skip_locked()` | `FOR SHARE SKIP LOCKED` | shared | silently omits exclusively-locked rows |

Pick **FOR UPDATE** when this session intends to write the row after
reading it (the canonical "read-then-write within one transaction"
pattern). Pick **FOR SHARE** when several sessions need to lock the
row against writers but tolerate each other reading concurrently
(typical for two services that each verify a balance before routing
a transfer).

```rust
// Exclusive: claim a job for processing.
let job = Job::objects()
    .filter(|f| f.status().eq(JobStatus::Pending))
    .order_by(|f| f.created_at().asc())
    .limit(1)
    .select_for_update()
    .skip_locked() // skip jobs another worker has already claimed
    .first(&mut tx)
    .await?;

// Shared: every checker locks the row against writers but not each other.
let balance = Account::objects()
    .filter(|f| f.id().eq(account_id))
    .select_for_share()
    .fetch_one(&mut tx)
    .await?;
```

### Pool-backed locks release immediately — wrap in `atomic`

Every row-level lock is scoped to the active transaction. A
pool-backed `DjogiContext` auto-commits each statement, so a lock
acquired on a pool-backed context releases the instant the implicit
transaction closes — **no mutual exclusion** between the fetch and any
follow-up step. Wrap correctness-sensitive lock chains in
`djogi::transaction::atomic` so the lock survives the read-then-decide
window.

### Contention modifiers are family-specific

`.nowait()` and `.skip_locked()` historically imply `FOR UPDATE` and
always set the FOR UPDATE family of variants — calling either after
`.select_for_share()` silently swaps the base lock back to FOR UPDATE.
For FOR SHARE contention semantics, use the dedicated
`.for_share_nowait()` / `.for_share_skip_locked()` methods. Mixing
families (e.g. `.select_for_update().for_share_nowait()`) is also
last-call-wins — the second call sets the lock outright.

---

## Terminal methods

Terminal methods consume the queryset, emit SQL via the framework's
`SqlAccumulator` (positional `$n` parameters over `tokio-postgres`),
and execute against a caller-provided `&mut DjogiContext`. The
context unifies pool and transaction handling: construct one with
`DjogiContext::from_pool(pool)` for pool-backed use, or pass the
context an enclosing transaction scope hands you.

| Method | Returns | Notes |
|---|---|---|
| `.fetch_all(&mut ctx)` | `Result<Vec<T>, DjogiError>` | Every matching row. Requires `T: FromPgRow`. |
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
`Regex(String)` routes to the Postgres `~` operator (case-sensitive
POSIX regex, server-side — see the closure-API `.regex` notes above
for the Postgres-feature framing); the closure API's `.iregex` (`~*`)
does not currently have a `Lookup` equivalent because no caller has
needed the runtime-decided form. Adding `Lookup::IRegex(String)` is
non-breaking when needed.

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

The returned count is `u64` — `tokio_postgres::Client::execute`'s
rows-affected return value passed through unchanged.

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
DDL-style reaches for `TRUNCATE` via `ctx.raw_execute`.

---

## Set operations (Phase 8.5)

`QuerySet<T>` exposes four SQL set operators — `union`, `union_all`,
`intersect`, and `except` — each of which combines two same-model
querysets into a `SetOpQuerySet<T>`. Both arms must carry the same `T:
Model`; the result rows decode through `T`'s existing `FromPgRow` impl.

```rust
use djogi::prelude::*;

// Each set operation consumes both arms, so construct fresh querysets for
// each call rather than trying to reuse a moved binding.

// UNION — de-duplicated union of both result sets.
let rows: Vec<Dog> = Dog::objects().filter(|f| f.status().eq(Status::Adopted))
    .union(Dog::objects().filter(|f| f.status().eq(Status::Fostered)))
    .fetch_all(&mut ctx).await?;

// UNION ALL — duplicate-preserving union.
let rows: Vec<Dog> = Dog::objects().filter(|f| f.status().eq(Status::Adopted))
    .union_all(Dog::objects().filter(|f| f.status().eq(Status::Fostered)))
    .fetch_all(&mut ctx).await?;

// INTERSECT — rows present in both arms (de-duplicated).
let rows: Vec<Dog> = Dog::objects().filter(|f| f.status().eq(Status::Adopted))
    .intersect(Dog::objects().filter(|f| f.status().eq(Status::Fostered)))
    .fetch_all(&mut ctx).await?;

// EXCEPT — rows in the left arm not in the right arm (de-duplicated).
let rows: Vec<Dog> = Dog::objects().filter(|f| f.status().eq(Status::Adopted))
    .except(Dog::objects().filter(|f| f.status().eq(Status::Fostered)))
    .fetch_all(&mut ctx).await?;
```

### Outer modifiers

`SetOpQuerySet<T>` exposes `order_by`, `limit`, and `offset` that apply to
the **combined** result — after the set operator:

```rust
let rows: Vec<Dog> = Dog::objects()
    .filter(|f| f.status().eq(Status::Adopted))
    .union(Dog::objects().filter(|f| f.status().eq(Status::Fostered)))
    .order_by(|f| f.name().asc())
    .limit(20)
    .offset(0)
    .fetch_all(&mut ctx)
    .await?;
```

Per-arm `.order_by(...)` / `.limit(...)` are legal and apply inside each
arm's parenthesised SQL. Outer ordering applies to the merged row set.

### Available terminals

| Method | Returns |
|---|---|
| `.fetch_all(&mut ctx)` | `Result<Vec<T>, DjogiError>` |
| `.first(&mut ctx)` | `Result<Option<T>, DjogiError>` |
| `.count(&mut ctx)` | `Result<i64, DjogiError>` |

### Invalid-arm validation

`DjogiError::SetOpArmInvalid` is returned at SQL-build time (before any
database round-trip) when an arm carries state incompatible with a set-op
shape:

- `.prefetch(...)` registrations
- `.select_related(...)` registrations
- `.select_for_update(...)` / `.nowait()` / `.skip_locked()` /
  `.select_for_share(...)` / `.for_share_nowait()` /
  `.for_share_skip_locked()` row locks
These leak structural shape (extra projections, locks) into a context where
the arm is emitted as a plain `SELECT t.* FROM t`. Silently dropping them
would be a correctness bug.

`DjogiError::SetOpOuterOrderingInvalid` fires when the outer `order_by`
carries an expression-form term Postgres rejects on set-operation `ORDER
BY` — for example, `order_by_distance(...)` (which lowers to
`ST_Distance(...)`). Postgres's grammar restricts set-operation outer `ORDER
BY` to column names or position numbers. Per-arm spatial ordering is still
legal; combined-result spatial ordering is not supported today.

### Nested composition

`union` / `union_all` / `intersect` / `except` are also available on
`SetOpQuerySet<T>`, so operations chain naturally:

```rust
a.union(b).intersect(c)
// emits: ((LEFT) UNION (RIGHT)) INTERSECT (c_select)
```

---

## INSERT SELECT — bulk copy (Phase 8.5)

`QuerySet::insert_into::<T, _, _>(...)` executes a typed
`INSERT INTO target (cols…) SELECT exprs… FROM source [WHERE …]` —
copying rows from one model's queryset into another model's table without
a round-trip through Rust.

```rust
use djogi::prelude::*;

let cutoff = /* some DateTime */;

// Archive completed orders.
let rows_copied: u64 = CompletedOrder::objects()
    .filter(|f| f.completed_at().lt(cutoff))
    .insert_into::<OrderArchive, _, _>(|target, source| vec![
        target.order_id().copy_from(source.id().as_insert_source()),
        target.title().copy_from(source.title().as_insert_source()),
        target.completed_at().copy_from(source.completed_at().as_insert_source()),
    ])
    .execute(&mut ctx)
    .await?;
```

The type parameters: `T` = target model (must be named at the call site —
nothing else in the expression carries it); the two `_` wildcards are the
closure type and return-shape type, inferred by rustc.

### Column mapping — `as_insert_source` and `copy_from`

`FieldRef::as_insert_source()` wraps a source-side field as an
`InsertSelectSource<S, V>`. It accepts only calls from the source field
bag (second closure argument).

`FieldRef::copy_from(source)` pairs a target-side field with a source
operand. Swapping sides is a compile error:

```rust
// OK — target field ← source field.
target.title().copy_from(source.title().as_insert_source())

// Compile error — source/target mismatch caught by the type system.
source.title().copy_from(target.title().as_insert_source())
```

Literal values are also valid source operands via `InsertSelectSource::literal`,
provided the value implements `Into<Expr<V>>` for the target column's type `V`.
For a `String` column this is straightforward:

```rust
target.note().copy_from(InsertSelectSource::literal("archived".to_string()))
```

For custom types such as enums, you must implement `Into<Expr<YourType>>` before
`literal` accepts the value — see the rustdoc for `InsertSelectSource::literal`
for the required bridge.

### Framework columns

The target's framework columns (`id`, `created_at`, `updated_at`) are
populated by their column-level `DEFAULT` clauses — they are never
included in the emitted INSERT column list unless explicitly mapped in the
closure. This matches `Model::create`'s contract.

### Rejected source states

`DjogiError::Validation` is returned at execution time — **before any SQL
is issued** — when the source queryset carries state incompatible with
INSERT SELECT, or when the column mapping itself is structurally invalid.
Validation runs even when the source queryset is `.none()`-derived so that
a programming error does not hide behind a silent `Ok(0)`.

**Column-mapping validations:**

- **Empty mapping** — the closure returned zero column pairs. Postgres
  rejects `INSERT INTO t () SELECT ...` as a syntax error; Djogi surfaces
  the diagnostic with the target table name before the SQL leaves the
  framework.
- **Duplicate target column** — the same target column appears more than
  once in the mapping (Postgres SQLSTATE `42701`). Pre-validated so the
  error message names the offending column rather than the raw SQLSTATE.

**Source-queryset state rejections:**

- `.prefetch(...)` registrations — prefetch is a post-fetch row-stitching
  step; INSERT SELECT returns no rows.
- `.select_related(...)` registrations — the join expands the SELECT list;
  the column mapping closure references single-model columns and cannot
  silently drop the join.
- `.select_for_update(...)` / `.nowait()` / `.skip_locked()` /
  `.select_for_share(...)` / `.for_share_nowait()` /
  `.for_share_skip_locked()` row locks —
  `SELECT ... FOR UPDATE` (or `... FOR SHARE`) inside INSERT SELECT is
  valid Postgres semantics (locking source rows for the archival
  duration), but the v0.1.0 surface does not compose row locks with
  INSERT SELECT. Drop the lock call before `.insert_into(...)`.
- `.distinct()` / `.distinct_on(...)` — deduplication inside INSERT SELECT
  is also valid Postgres, but `DISTINCT ON` requires the deduplication
  columns to appear first in `ORDER BY` and in the SELECT projection —
  neither of which the closure-built mapping guarantees. All non-default
  distinct modes are rejected in v0.1.0.

### Return value and RETURNING

The terminal returns the affected row count (`u64`). `RETURNING` for
INSERT SELECT is not in v0.1.0 — follow up with a SELECT on the target
when you need the inserted rows back.

---

## Recursive / tree queries

For self-referential data — file-system trees, org charts, message
threads, biological pedigrees — Djogi exposes a typed recursive query
surface that emits a Postgres `WITH RECURSIVE ... SELECT * FROM ...`
CTE under the hood. No raw SQL, no `JUSTIFICATION` bypass.

The shipped surface (Phase 8-Zero Cluster B, GH #65) has two layers:

### Tree-edge sugar — `Model::tree_descendants` / `Model::tree_ancestors`

When the model declares `#[model(tree_edge = "parent_id")]` (or any
self-FK column name), `Model::tree_descendants(root_id)` and
`Model::tree_ancestors(node_id)` resolve that edge automatically:

```rust
use djogi::prelude::*;

#[model(table = "categories", tree_edge = "parent_id")]
#[derive(Debug, Clone)]
pub struct Category {
    pub name: String,
    pub parent_id: Option<ForeignKey<Category>>,
}

// Every descendant of `root_id`, in DB-order.
let subtree: Vec<Category> = Category::tree_descendants(root_id)?
    .fetch_all(&mut ctx)
    .await?;

// Every ancestor of `node_id`, ending at the root.
let chain: Vec<Category> = Category::tree_ancestors(node_id)?
    .fetch_all(&mut ctx)
    .await?;
```

Both sugar methods return `Result<RecursiveQuerySet<T>, DjogiError>`.
`Err(DjogiError::Validation)` is returned when the model has no
`#[model(tree_edge = "...")]` declaration — the error message names
the model and points at the explicit-path API below.

### Explicit-path API — `QuerySet::tree_descendants` / `tree_ancestors`

For multi-edge graphs (e.g., a pedigree where both `mother_id` and
`father_id` matter) reach for the lower-level
`QuerySet::tree_descendants` / `tree_ancestors` with an explicit typed
`RelationPath`:

```rust
// `ElephantRelated::mother()` is the generated `RelationPath` for the
// `mother_id` self-FK column (`_id` suffix is stripped by the macro).
let mothers_descendants: RecursiveQuerySet<Elephant> = Elephant::objects()
    .tree_descendants(ElephantRelated::mother(), root_id);
```

`RecursiveQuerySet` ships three optional modifiers beyond the base
`filter` / `order_by`:

- **`.with_max_depth(n: u32)`** — adds `AND parent.depth < $n` in the
  recursive term. When omitted, the walk runs to natural exhaustion or
  until the always-on `CYCLE` clause fires.
- **`CYCLE id SET is_cycle USING cycle_path`** — emitted unconditionally
  on every recursive query. Postgres marks cyclic paths and stops
  re-visiting them, so a corrupt self-FK graph never hangs. The
  de-cycling column is framework-internal (`__djogi_`-prefixed) and is
  stripped before rows are decoded.
- **`.search_breadth_first_by(field)`** / **`.search_depth_first_by(field)`**
  — emit Postgres' `SEARCH BREADTH FIRST BY <col>` / `SEARCH DEPTH FIRST
  BY <col>` annotation and prepend the framework-generated
  `ORDER BY __djogi_search_seq` on the outer SELECT so callers see
  traversal order without writing the sort term manually. These are
  mutually exclusive (last call wins).

All three stack with `.filter(...)` and `.order_by(...)`, which append
tiebreakers after the search-sequence column when both are present.

### Materialised closure — `Model::materialize_closure`

For repeat-read traversals (kinship lookups, permission inheritance,
tree-coloring jobs) the recursive CTE is run once into a denormalised
closure table:

```rust
use djogi::prelude::*;

#[model(table = "category_ancestries")]
#[derive(Debug, Clone)]
pub struct CategoryAncestry {
    pub category_id: ForeignKey<Category>,
    pub ancestor_id: ForeignKey<Category>,
    pub depth: i32,
    // COUNT(*) — always BIGINT on Postgres; must be i64.
    pub path_count: i64,
}

impl djogi::query::ClosureModel for CategoryAncestry {
    type Source = Category;

    fn source_column() -> &'static str { "category_id" }
    fn ancestor_column() -> &'static str { "ancestor_id" }
    fn depth_column() -> &'static str { "depth" }
    fn path_count_column() -> &'static str { "path_count" }
}

// Walks the recursive CTE once and writes one row per
// (descendant, ancestor, depth, path_count) tuple. `UNION ALL`
// preserves multi-path multiplicity for kinship-style summations.
let summary = Category::materialize_closure::<CategoryAncestry>(
    &mut ctx,
    Default::default(),
).await?;
```

Adopters that hit the closure table from a queryset use the pair-tuple
substrate below to JOIN both sides of a candidate pair to the
materialised closure in one round-trip.

---

## Pair-tuple closure self-joins

Some queries are inherently pair-shaped: "for every (left, right) pair
in this table, compute something that depends on a JOIN of both rows
against a third table." Wright F kinship over a materialised pedigree
closure is the canonical example. Djogi exposes a typed pair-tuple
substrate so adopters write these queries without raw SQL.

The shipped surface (Phase 8.5 Cluster 4A, GH #99) is rooted at
`QuerySet::self_pairs()`:

```rust
use djogi::prelude::*;
use djogi::query::PairClosureKinshipSum;

// Every (female, male) candidate pair with its Wright F coefficient
// in a single round-trip.
let kinship_pairs: Vec<((Elephant, Elephant), f64)> = Elephant::objects()
    .self_pairs()                                          // (L, R = L)
    .filter_left(|f| f.id().in_(female_ids))               // narrow left
    .filter_right(|m| m.id().in_(male_ids))                // narrow right
    .left_join_closure_pair::<ElephantAncestry>()          // la / ra
    .annotate(|_l, _r| PairClosureKinshipSum::<ElephantAncestry>::new())
    .fetch_all(&mut ctx)
    .await?;
```

What the substrate emits:

```sql
SELECT l.<cols> AS l_<cols>, r.<cols> AS r_<cols>,
       COALESCE(SUM(la.path_count * ra.path_count
                    * POWER(0.5, la.depth + ra.depth + 1)), 0)
       ::float8 AS __djogi_agg_0
FROM   elephants AS l
CROSS JOIN elephants AS r
LEFT JOIN elephant_ancestries AS la ON la.elephant_id = l.id
LEFT JOIN elephant_ancestries AS ra ON ra.elephant_id = r.id
                                   AND ra.ancestor_id = la.ancestor_id
WHERE  l.id <> r.id
  AND  l.id = ANY($1) AND r.id = ANY($2)
GROUP BY l.id, r.id;
```

Surface notes:

- **`.filter_left(...)` / `.filter_right(...)`** accept the same closure
  shape as `QuerySet::filter` — typed `FieldRef<L, V>` / `FieldRef<R, V>`
  handles, fluent `and_with` / `or_with` combinators.
- **`.left_join_closure_pair::<C>()`** requires `C` to be a closure-table
  model (typically the output of `Model::materialize_closure::<C>`). The
  per-pair `GROUP BY l.id, r.id` is auto-emitted because the closure-pair
  annotations report `requires_closure_pair_join()` at validation time.
- **`PairClosureKinshipSum<C>`** is the typed annotation slot for the
  Wright F sum. Other pair-shaped aggregates live in `djogi::query` next
  to it; adopters can add their own by implementing the
  `PairClosureAnnotation` trait. The aggregate output lands under the
  framework-reserved `__djogi_agg_0` alias and decodes to the
  `(_, _, T)` slot of the `Vec<((L, R), T)>` terminal.
- **`.include_equal_pk()`** opts in to the `l.id = r.id` self-pair —
  useful for diagonal kinship lookups; the default `WHERE l.id <> r.id`
  drops them.

Composite scores that mix pair-aggregate output with Rust-side state
(score from kinship × Rust-side overlap × Rust-side age product) land
their final ranking in Rust; the typed pair-tuple `qualify(...)` window
surface accepts column references only on its
`partition_by_pair` / `order_by_pair_desc` methods, not arbitrary
`Expr<f64>` derived from external state. A future slice that adds an
`Expr`-based pair-side `order_by` is tracked on #99's substrate
roadmap.

### Pair-side spatial overlap — `PairAreaOverlapRatio<L, R>`

For the spatial pair-shape — "what fraction of `L.territory` overlaps
`R.territory`?" — the pair-tuple substrate composes with the typed
spatial annotation
[`PairAreaOverlapRatio<L, R>`](https://docs.rs/djogi/latest/djogi/query/struct.PairAreaOverlapRatio.html):

```rust
use djogi::prelude::*;
use djogi::query::PairAreaOverlapRatio;

// Per-pair territory overlap ratio across every herd-pair.
let overlaps: Vec<((Herd, Herd), f64)> = Herd::objects()
    .self_pairs()
    .include_equal_pk()  // same-herd pairs report 1.0
    .annotate(|l, r| PairAreaOverlapRatio::new(l.territory(), r.territory()))
    .fetch_all(&mut ctx)
    .await?;
```

The annotation emits
`COALESCE(ST_Area(ST_Intersection(l.col::geometry, r.col::geometry)::geography), 0)::float8
 / NULLIF(ST_Area(l.col::geography), 0)::float8` per pair. The ratio is
left-normalised and asymmetric — see the
[spatial guide's pair-side section](./spatial.md#pair-side-territory-overlap-phase-85-99)
for the full SQL shape, NULL/disjoint/degenerate semantics, and the
column-type rules.

---

## Cache-bound terminals — `.cache(&pool)?`

Repeat-read query patterns (request handlers re-reading the same row
across endpoints, periodic scoring jobs re-evaluating the same
candidate set) amortise the DB round-trip by binding a queryset to a
`Punnu<T>` L1 identity-map pool. Djogi's typed surface for this
(Phase 8.5 Cluster 4A, GH #108) is the `.cache(&pool)?` modifier:

```rust
use djogi::prelude::*;

// `#[derive(Model)]` auto-emits `impl Cacheable for Post` on every
// default-deriving model, and the boot hook registers a per-context
// `Punnu<Post>` at `DjogiContext::from_pool` time — so adopters get
// the pool handle through `ctx.punnu::<Post>()` without manual glue.
let pool = ctx
    .punnu::<Post>()
    .expect("Punnu<Post> registered by the boot hook on default-derive")
    .clone();

let recent: Vec<Post> = Post::objects()
    .filter(|f| f.published().eq(true))
    .order_by(|f| f.created_at().desc())
    .limit(20)
    .cache(&pool)?                              // ← portable-gated binding
    .fetch_all(&mut ctx)
    .await?;

// Subsequent lookups against the same pool hit L1, no DB round-trip:
let arc: Option<std::sync::Arc<Post>> = pool.get(&recent[0].id);
```

Surface contract:

- `.cache(&pool)` returns
  `Result<CachedPortableQuerySet<'_, T>, (QuerySet<T>, PortablePredicateError)>`.
  The error variant returns ownership of the original queryset so the
  caller can fall back to a non-cached path on the same call site.
- Cache binding is **portable-gated**: every filter must reduce to
  `sassi::BasicPredicate<T>`. Ordinary closure filters using typed
  `FieldRef<T, V>` lookups (`eq`, `lte`, `in_`, …) are portable today.
  Filters that smuggle raw `Condition` payloads, JSONB-path lookups
  beyond the typed surface, or expression-backed comparisons are
  intentionally non-portable.
- Cache binding only applies to row-returning terminals (`fetch_all`,
  `fetch_one`, `first`); `count` runs unchanged because `COUNT(*)`
  returns no rows for the identity map to absorb.
- Row mirroring happens at row-decode time in the existing terminal
  pipeline — there is no separate post-fetch insertion loop, and no
  "insert one row that succeeded but lost the second" failure mode
  visible to the caller.
- `Punnu::insert` is invalidated automatically when `Model::create`,
  `Model::save`, or `Model::delete` runs through djogi's hook
  machinery (cluster 8δ T7.5). Adopters do not maintain a manual
  write-through.

For adopters who need to inspect whether a queryset is
cache-eligible ahead of binding, `QuerySet::try_portable()` returns
the same `Result<PortableQuerySet<T>, ...>` as `.cache(...)` but
without consuming the pool handle.

---

## Raw escape hatch

When `QuerySet` can't express the query — recursive CTEs,
set-returning functions, bespoke joins beyond what `select_related`
covers — drop to `ctx.raw_query` / `ctx.raw_scalar` / `ctx.raw_execute`
on `DjogiContext` only as a justified raw-SQL bypass: the enclosing item
must carry `#[djogi::deliberately_bypass_convention_with_raw_sql]` and an
adjacent `// JUSTIFICATION ...` comment. See [Models §Rule 3][models-raw]
for the raw-query surface.

The raw path sits next to the typed one — a query that starts as
`QuerySet` can pick up a raw tail when a feature isn't shipped yet,
without migrating the entire call site.

---

## MERGE INTO

`MERGE` (Phase 8.5, GH #178) is a powerful single statement that synchronizes
a source relation (any `QuerySet<S>`) into a target table. It can perform
`INSERT`, `UPDATE`, or `DELETE` actions based on whether rows match a join
condition.

Common use cases include:
- **Bulk Upsert**: Insert new rows and update existing ones from an external feed.
- **Update if Changed**: Only update target rows when the source value actually differs (using `.is_distinct_from_source()`).
- **Soft Deletion**: Flag target rows as "inactive" if they are no longer present in the source relation.

### Basic Syntax

```rust
use djogi::prelude::*;

source_qs.merge_into::<Target, _, _>(|target, source| {
    target.external_id().merge_on_eq(source.external_id())
})
.when_matched_and_update(Some(target.payload().is_distinct_from_source(source.payload())), vec![
    target.payload().merge_copy_from(source.payload()),
])
.when_not_matched_then_insert(None, vec![
    target.external_id().merge_insert_from(source.external_id()),
    target.payload().merge_insert_from(source.payload()),
])
.execute(&mut ctx).await?;
```

### Convenience: `when_matched_update_changed`

Building a manual "update if changed" condition for multiple columns is tedious.
Djogi provides a convenience helper that automatically builds an
`IS DISTINCT FROM` chain for you:

```rust
.when_matched_update_changed(vec![
    target.field_a().merge_copy_from(source.field_a()),
    target.field_b().merge_copy_from(source.field_b()),
])
// Emits: WHEN MATCHED AND (tgt.field_a IS DISTINCT FROM src.field_a OR tgt.field_b IS DISTINCT FROM src.field_b)
```

### Validations & Safety

To prevent accidental broad data loss or corruption, Djogi enforces several
safety invariants on the `MERGE` surface:

1. **Structural `.none()` Rejection**: If you attempt to run a `MERGE` with
   source.none() and a `WHEN NOT MATCHED BY SOURCE` branch is present, Djogi
   will return an error. This prevents accidentally deleting your entire target
   table because the source happened to be empty.
2. **Target-only predicates for BY SOURCE**: Branches that act on missing source
   rows cannot reference source fields in their conditions.
3. **Auto-stamping**: All `UPDATE` actions in a `MERGE` automatically append
   `updated_at = now()` to the set list, maintaining Djogi's audit integrity.
4. **Source State**: The source queryset must not carry state that is incompatible
   with a `USING` subquery (e.g., prefetch, cache, or locks).
