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

## Supported element types

The following Rust types are accepted as array element types:

| Rust type | Postgres column type |
|---|---|
| `String` | `TEXT[]` |
| `i16` | `SMALLINT[]` |
| `i32` | `INTEGER[]` |
| `i64` | `BIGINT[]` |
| `f32` | `REAL[]` |
| `f64` | `DOUBLE PRECISION[]` |
| `bool` | `BOOLEAN[]` |
| `djogi::DateTime` / `time::OffsetDateTime` | `TIMESTAMPTZ[]` |
| `djogi::Date` / `time::Date` | `DATE[]` |
| `uuid::Uuid` | `UUID[]` |
| `rust_decimal::Decimal` | `NUMERIC[]` |
| `djogi::HeerId` | `BIGINT[]` |
| `djogi::RanjId` | `UUID[]` |

Any other element type is a compile-time error. A `Vec<MyNewtype>` requires that
`MyNewtype` implements `DjogiSqlType` for the column type (so Djogi can write the
migration) **and** `IntoArrayFilterValue` (so array operators work). The
`IntoArrayFilterValue` trait is sealed — extending it to adopter-defined newtypes
or enums requires a framework change and a corresponding `DjogiSqlType` impl.

---

## Contract

| Method | SQL | Meaning |
|---|---|---|
| `.contains(values)` | `col @> $1` | Column array contains every element in `values` |
| `.contained_by(values)` | `col <@ $1` | Every element of the column array is in `values` |
| `.overlap(values)` | `col && $1` | Column array and `values` share at least one element |
| `.len()` | `array_length(col, 1)` | Number of elements; returns an `Expr<i32>` for integer comparisons |

`values` in `contains`, `contained_by`, and `overlap` takes a **slice reference**
`&[V]` where `V` matches the column's element type. The argument is bound as a
Postgres array parameter.

`len()` returns `Expr<i32>` and is used as a left-hand side: `f.tags().len().gt(3_i32)`
emits `array_length(tags, 1) > $1`.

---

## Routing to array operators

The `contains`, `contained_by`, and `overlap` methods live on
[`ExplicitPgPredicateField`](crate::query::ExplicitPgPredicateField), not on
`DjogiField` directly. These operators produce Postgres-only predicates that
cannot be evaluated in Punnu's in-memory cache; calling `.explicit_pg_predicate()`
makes the portability trade-off explicit.

```rust
AuditLog::objects()
    .filter(|f| f.touched_ids().explicit_pg_predicate().contains(&ids))
```

`len()` is the exception — it returns `Expr<i32>` and is available directly on
`DjogiField<M, Vec<V>>`:

```rust
AuditLog::objects()
    .filter(|f| f.tags().len().gt(3_i32))
```

---

## Example

```rust
use djogi::prelude::*;

#[model(table = "articles")]
#[derive(Debug, Clone)]
pub struct Article {
    pub title: String,
    pub tags: Vec<String>,
    pub reviewer_ids: Vec<djogi::HeerId>,
}

async fn example(pool: &DjogiPool) -> Result<(), DjogiError> {
    let mut ctx = DjogiContext::from_pool(pool.clone());

    // Find articles tagged with both "rust" AND "postgres".
    let both = Article::objects()
        .filter(|f| {
            f.tags()
                .explicit_pg_predicate()
                .contains(&["rust".to_string(), "postgres".to_string()])
        })
        .fetch_all(&mut ctx).await?;
    // WHERE tags @> ARRAY[$1, $2]

    // Find articles whose tag set is fully within an allowed list.
    let allowed = ["rust".to_string(), "async".to_string(), "postgres".to_string()];
    let contained = Article::objects()
        .filter(|f| f.tags().explicit_pg_predicate().contained_by(&allowed))
        .fetch_all(&mut ctx).await?;
    // WHERE tags <@ ARRAY[$1, $2, $3]

    // Find articles that share at least one reviewer with a given set.
    let known_ids: Vec<djogi::HeerId> = vec![];
    let overlapping = Article::objects()
        .filter(|f| f.reviewer_ids().explicit_pg_predicate().overlap(&known_ids))
        .fetch_all(&mut ctx).await?;
    // WHERE reviewer_ids && ARRAY[...]

    // Find articles with more than three tags.
    let long_tagged = Article::objects()
        .filter(|f| f.tags().len().gt(3_i32))
        .fetch_all(&mut ctx).await?;
    // WHERE array_length(tags, 1) > $1

    let _ = (both, contained, overlapping, long_tagged);
    Ok(())
}
```

---

## Common Patterns

### Text arrays vs integer arrays vs HeerId arrays

Any `V` that appears in the [supported element types table](#supported-element-types)
can be the element type of a `Vec<V>` column. Common choices:

- `Vec<String>` — text tags, labels, permission strings
- `Vec<i32>` / `Vec<i64>` — integer IDs, status codes
- `Vec<djogi::HeerId>` — references to other model rows stored denormalised
- `Vec<f64>` — numeric scores or weights
- `Vec<rust_decimal::Decimal>` — monetary or precise decimal arrays

The operators (`contains`, `contained_by`, `overlap`) work identically for all
element types. The type system enforces that the argument's element type matches
the column's element type at compile time.

### Combining array conditions

Array conditions compose with all other `QuerySet` filter operations using the
standard `and_with` / `or_with` combinators:

```rust
Article::objects()
    .filter(|f| {
        f.tags()
            .explicit_pg_predicate()
            .contains(&["rust".to_string()])
            .and_with(f.tags().len().gte(2_i32))
    })
    .fetch_all(&mut ctx).await?;
// WHERE (tags @> ARRAY[$1]) AND (array_length(tags, 1) >= $2)
```

### GIN indexes

The `@>`, `<@`, and `&&` operators benefit from a GIN index on the column.
Annotate the field with `#[field(index = "gin")]`; the descriptor-driven
migration composer emits the GIN index. Hand-written SQL is only needed when you
deliberately bypass Djogi's migration surface:

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
`ALL($1)`, unnest-based joins, or element-position access — drop to raw SQL.
The `raw_*` methods live on the sealed `djogi::__bypass::RawAccessExt`
extension trait, so every call site must decorate the enclosing item with
`#[djogi::deliberately_bypass_convention_with_raw_sql]` and pair it with an
adjacent `// JUSTIFICATION (djogi#<n>): ...` comment naming the typed-surface
gap (see [Raw SQL escape hatches](../spec/raw-sql-escape-hatches.md)).

```rust
use djogi::prelude::*;

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#234): scalar-vs-array `ANY($1)` not exposed by QuerySet.
async fn articles_tagged_with(
    ctx: &mut DjogiContext,
    tag: &str,
) -> djogi::Result<Vec<Article>> {
    // ANY scalar pattern: "is the given value in the column's array?"
    let found: Vec<Article> = ctx.raw_query(
        "SELECT * FROM articles WHERE $1 = ANY(tags)",
        &[&tag],
    ).await?;
    Ok(found)
}
```

`ANY($1)` is a Postgres scalar-vs-array comparison, not the same as
`col @> ARRAY[$1]` — they test the opposite direction. Use the typed
`.explicit_pg_predicate().contains(vec![v])` form when you mean "column array
contains this value", which emits `col @> ARRAY[$1]`.
