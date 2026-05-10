> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Testing Conventions

This document covers conventions that apply to all test fixtures in the
djogi repository: integration tests under `tests/integration/`, internal
tests under `tests/internal/`, and SQL seed scripts under `seeds/`.

For the raw SQL bypass harness (when and how `raw_*` APIs are
permitted), see [Raw SQL Escape Hatches](./raw-sql-escape-hatches.md).

## 1. Typed surface first

Every integration test under `tests/integration/` must build and
inspect database state through djogi's typed surface:

- `#[djogi::djogi_test(sync_models = [Model, ...])]` — provisions the
  per-test schema by routing through `djogi::testing::sync_models`,
  which projects each descriptor through `pk_default_sql` before
  dispatching DDL. The projection layer is always in the call chain.
- `Model::create` / `Model::save` / `Model::delete` — all row writes.
- `Model::objects()` and the `QuerySet` — all row reads.
- `djogi::transaction::atomic` — transaction management.

Directly calling `raw_execute`, `raw_ddl`, `raw_query`, or any other
`RawAccessExt` method from an ordinary integration test is prohibited.
The single exception per API is one dedicated pin test under `tests/pin/`
that exercises that API's own behaviour. See [Raw SQL Escape
Hatches](./raw-sql-escape-hatches.md) for the full list.

## 2. Self-referential seed convention

### The problem

Postgres gives each statement a snapshot of currently visible rows taken at the
moment that statement begins. A statement cannot see rows that it is
itself inserting. This means the following seed shape is broken:

```sql
-- ❌ BROKEN — the subquery snapshot does not include the rows being
--    inserted by this same statement.  With NOT NULL FK: fails with a
--    foreign-key-violation error.  With nullable FK: silently inserts
--    NULL for every parent_id, degrading tree queries and benchmarks
--    without raising an error.
INSERT INTO category (name, parent_id)
SELECT 'child_' || g,
       (SELECT id FROM category WHERE name = 'root')
FROM generate_series(1, 50) AS g;
```

The nullable-FK case is especially dangerous: the INSERT succeeds, the
fixture looks plausible, but every `parent_id` is `NULL`. Tree queries,
graph traversals, and benchmarks that depend on a connected hierarchy
silently measure a flat list instead.

### The convention

Split self-referential seeds across at least two statements:

**Step 1 — insert roots** (no self-reference needed):

```sql
INSERT INTO category (name, parent_id)
VALUES ('root', NULL);
```

**Step 2 — insert flat child rows with the self-reference column
unset** (or explicitly `NULL` when the column is nullable):

```sql
-- Inserts the children without linking them to the parent yet.
-- Captures an application-generated label so Step 3 can resolve
-- the parent id without an id-by-id subquery.
INSERT INTO category (name, parent_id)
SELECT 'child_' || g, NULL
FROM generate_series(1, 50) AS g;
```

**Step 3 — resolve parent links in a later `UPDATE … FROM` over the prior
statement-visible rows**:

```sql
-- Rows from the previous statements in this seed are now visible.
UPDATE category AS child
SET parent_id = root.id
FROM category AS root
WHERE root.name = 'root'
  AND child.name LIKE 'child_%';
```

Because the UPDATE runs as a separate statement, Postgres takes a fresh
snapshot that includes rows written by all previous statements in the
current transaction. The parent is visible and the FK resolves correctly.

### When does this apply?

Apply this convention whenever a seed or fixture inserts rows into a
table that also references itself (directly or through a cycle):

- Self-referencing FK columns (`parent_id`, `manager_id`, `reply_to_id`,
  `predecessor_id`, …).
- Closure / adjacency-list tables where the same row acts as both child
  and parent depending on depth.
- SQL bench fixtures that need a connected graph to measure traversal
  or aggregate behaviour.

The same principle applies to **cross-table cycles** (`table_a`
references `table_b` which references `table_a`): insert both sides
with the cycle-forming FK column `NULL`, then fix the cycle with an
`UPDATE … FROM` after both sides are visible in the next statement.

### Djogi-typed test helpers

When seeding inside a Rust integration test through the typed surface,
create rows individually and pass the already-returned parent back as
the FK argument:

```rust
// ✓ Correct: the root row is written before the child is created.
let root = Category::create(&mut ctx, Category {
    id: <HeerId as PrimaryKey>::sentinel(),
    created_at: DateTime::UNIX_EPOCH,
    updated_at: DateTime::UNIX_EPOCH,
    name: "root".into(),
    parent_id: None,
}).await?;

let child = Category::create(&mut ctx, Category {
    id: <HeerId as PrimaryKey>::sentinel(),
    created_at: DateTime::UNIX_EPOCH,
    updated_at: DateTime::UNIX_EPOCH,
    name: "child_1".into(),
    parent_id: Some(ForeignKey::new(root.id)), // root.id is already visible
}).await?;
```

`Model::create` issues a single `INSERT … RETURNING` per call, so the
returned id is already visible and immediately safe to use as a FK in the
next call.

For bulk typed fixtures, insert an entire level of the hierarchy before
proceeding to the next level. Never use `Model::create` in a loop where
the `parent_id` argument references a row from the *same* loop
iteration that has not yet been returned and made visible to the next
statement.
