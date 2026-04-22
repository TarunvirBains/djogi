> [Back to Guides](./index.md) · [Back to README](../../ReadMe.MD)

Spec: [`docs/spec/models.md`](../spec/models.md) — Phase 5 array field operators.

# Array Fields

Djogi supports `Vec<V>` as a model field type backed by a Postgres one-dimensional
array column. Four typed operators are available on `FieldRef<M, Vec<V>>`:
`contains`, `contained_by`, `overlap`, and `len`. All four emit native Postgres
array operators (`@>`, `<@`, `&&`, `array_length`), which Postgres can accelerate
with a GIN index.

Multi-dimensional arrays are not a supported field type — all array columns are
one-dimensional.

---

## Contract

| Method | SQL | Meaning |
|---|---|---|
| `.contains(values)` | `col @> $1` | Column array contains every element in `values` |
| `.contained_by(values)` | `col <@ $1` | Every element of the column array is in `values` |
| `.overlap(values)` | `col && $1` | Column array and `values` share at least one element |
| `.len()` | `array_length(col, 1)` | Number of elements; returns a `LenRef` that accepts integer comparisons |

`values` in `contains`, `contained_by`, and `overlap` accepts any
`IntoIterator<Item = V>` where `V` matches the column's element type. The
argument is bound as a Postgres array parameter.

`len()` is used as a left-hand side: `f.tags().len().gt(3)` emits
`array_length(tags, 1) > 3`.

---

## Example

```rust
use djogi::prelude::*;

#[model(table = "articles")]
#[derive(Debug, Clone)]
pub struct Article {
    pub title: String,
    pub tags: Vec<String>,
    pub reviewer_ids: Vec<i64>,
}

async fn example(pool: &DjogiPool) -> Result<(), DjogiError> {
    let mut ctx = DjogiContext::from_pool(pool.clone());

    // Find articles tagged with both "rust" AND "postgres".
    let both = Article::objects()
        .filter(|f| f.tags().contains(vec!["rust".to_string(), "postgres".to_string()]))
        .fetch_all(&mut ctx).await?;
    // WHERE tags @> ARRAY[$1, $2]

    // Find articles whose tag set is fully within an allowed list.
    let allowed = vec!["rust".to_string(), "async".to_string(), "postgres".to_string()];
    let contained = Article::objects()
        .filter(|f| f.tags().contained_by(allowed.clone()))
        .fetch_all(&mut ctx).await?;
    // WHERE tags <@ ARRAY[$1, $2, $3]

    // Find articles that share at least one reviewer with a given set.
    let known_ids = vec![7_i64, 12, 99];
    let overlapping = Article::objects()
        .filter(|f| f.reviewer_ids().overlap(known_ids.clone()))
        .fetch_all(&mut ctx).await?;
    // WHERE reviewer_ids && ARRAY[$1, $2, $3]

    // Find articles with more than three tags.
    let long_tagged = Article::objects()
        .filter(|f| f.tags().len().gt(3_i64))
        .fetch_all(&mut ctx).await?;
    // WHERE array_length(tags, 1) > $1

    let _ = (both, contained, overlapping, long_tagged);
    Ok(())
}
```

---

## Common Patterns

### Text arrays vs integer arrays vs custom type arrays

Any `V` that implements `postgres_types::ToSql` + `postgres_types::FromSql`
can appear as an array element type. Common choices:

- `Vec<String>` — text tags, labels, permission strings
- `Vec<i32>` / `Vec<i64>` — integer IDs, status codes
- `Vec<f64>` — numeric scores or weights
- `Vec<YourDjogiEnum>` — typed enum arrays (requires `DjogiEnum` to derive
  `postgres_types::ToSql` / `FromSql`, which `#[derive(DjogiEnum)]` provides)

The operators (`contains`, `contained_by`, `overlap`) work identically for all
element types. The type system enforces that the argument's element type matches
the column's element type at compile time.

### Combining array conditions

Array conditions compose with all other `QuerySet` filter operations using the
standard `and_with` / `or_with` combinators:

```rust
Article::objects()
    .filter(|f| {
        f.tags().contains(vec!["rust".to_string()])
            .and_with(f.tags().len().gte(2_i64))
    })
    .fetch_all(&mut ctx).await?;
// WHERE (tags @> ARRAY[$1]) AND (array_length(tags, 1) >= $2)
```

### GIN indexes

The `@>`, `<@`, and `&&` operators benefit from a GIN index on the column.
GIN index emission is deferred to Phase 7. In the meantime, add the index
manually in your migration:

```sql
CREATE INDEX articles_tags_gin ON articles USING gin(tags);
```

You can annotate the field with `#[field(index = "gin")]` today — the
descriptor records the intent for Phase 7's migration differ to pick up:

```rust
#[field(index = "gin")]
pub tags: Vec<String>,
```

### When to use a join table instead

Array columns are best for small, stable sets (3–20 elements per row) where
ORDER does not matter and you do not need per-element metadata. Consider an
explicit junction model instead when:

- The element set is large or unbounded (hundreds of elements per row).
- You need to ORDER or LIMIT the element set independently.
- Each element carries its own columns (e.g., a `Tag` model with a `color`
  field).

See the [relations guide](./relations.md) for the explicit-through M2M pattern.

---

## Escape Hatch

For array predicates that the typed operators do not cover — `ANY($1)`,
`ALL($1)`, unnest-based joins, or element-position access — drop to raw SQL:

```rust
// ANY scalar pattern: "is the given value in the column's array?"
let found = ctx.raw_query::<Article>(
    "SELECT * FROM articles WHERE $1 = ANY(tags)",
    &[&"rust"],
).await?;
```

`ANY($1)` is a Postgres scalar-vs-array comparison, not the same as
`col @> ARRAY[$1]` — they test the opposite direction. Use the typed
`.contains(vec![v])` form when you mean "column array contains this value",
which emits `col @> ARRAY[$1]`.
