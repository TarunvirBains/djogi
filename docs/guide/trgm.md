> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Trigram Similarity (pg_trgm)

The `trgm` feature exposes Postgres's `pg_trgm` extension as a first-class
typed surface on text columns. Two operations are provided:

- `.explicit_pg_predicate().trgm_similar_to(pattern, threshold)` — a
  `WHERE`-clause predicate (Postgres-specific, not portable to Punnu).
- `.trgm_similarity(pattern)` — a scored `Expr<f64>` for `ORDER BY` / `annotate`.

Together these cover the principal pg_trgm use-case: ranked partial-match
search over user-visible strings (profile bios, tags, autocomplete, name
lookups). Unlike `ILIKE`, trigram similarity is language-agnostic and performs
acceptably without prefix or suffix anchoring.

---

## Enabling the feature

Add `trgm` to your `djogi` dependency in `Cargo.toml`:

```toml
[dependencies]
djogi = { version = "...", features = ["trgm"] }
```

**Extension requirement:** `pg_trgm` must be installed in the target Postgres
database before any query using the `similarity()` function can execute:

```sql
CREATE EXTENSION IF NOT EXISTS pg_trgm;
```

If your application role does not have `CREATE EXTENSION` privileges, a
database administrator must install it. When the migration that introduces a
trgm index is applied, the runner reads the `extension_dependency` field on
the `IndexSpec` and surfaces a clear error if `pg_trgm` is absent (see the
[index section](#declaring-a-gin-or-gist-index) below).

---

## Predicate — `trgm_similar_to`

```rust
ExplicitPgPredicateField<M, String>::trgm_similar_to(
    pattern: impl Into<String>,
    threshold: f64,
) -> Condition
```

Filters rows where the column value is at least `threshold`-similar to
`pattern`. Similarity is a `f64` in `[0.0, 1.0]`; a threshold of `0.3` is a
common starting point for fuzzy name lookups.

`trgm_similar_to` is a Postgres-specific predicate (it requires the `pg_trgm`
extension and is not evaluable in Punnu's in-memory cache). Reach it via
`.explicit_pg_predicate()` — the same pattern as `regex` and `iregex`:

```rust
use djogi::prelude::*;

#[model(app = "profiles", table = "user_profile")]
pub struct UserProfile {
    pub name: String,
    pub bio: String,
}

// Find profiles whose bio is at least 30 % similar to "machine learning".
let matches = UserProfile::objects()
    .filter(|f| f.bio().explicit_pg_predicate().trgm_similar_to("machine learning", 0.3))
    .fetch_all(&mut ctx)
    .await?;
```

Generated SQL:

```sql
SELECT id, created_at, updated_at, name, bio
FROM user_profile
WHERE similarity(bio, $1) >= $2
-- binds: $1 = "machine learning", $2 = 0.3
```

Both the `pattern` and `threshold` are positional bind parameters — no user
text is ever interpolated into SQL.

---

## Score expression — `trgm_similarity`

```rust
DjogiField<M, String>::trgm_similarity(
    pattern: impl Into<String>,
) -> Expr<f64>
```

Returns an `Expr<f64>` that evaluates the `pg_trgm` `similarity()` function per
row. Use it in `filter_expr` to build threshold comparisons through the typed
`Expr<T>` API, or in `annotate` to surface the score as a named computed column.

### Score-based filtering with `filter_expr`

```rust
use djogi::prelude::*;

let matches = UserProfile::objects()
    .filter_expr(|f| {
        f.bio()
            .trgm_similarity("machine learning")
            .gte(Expr::literal(0.3_f64))
    })
    .fetch_all(&mut ctx)
    .await?;
```

Generated SQL: `WHERE similarity(bio, $1) >= $2`

This is equivalent to `.explicit_pg_predicate().trgm_similar_to(pattern, threshold)`.
Use the `filter_expr` form when composing with other expressions or when the
threshold itself comes from a sub-expression.

### As a named annotation

```rust
use djogi::prelude::*;

let annotated = UserProfile::objects()
    .filter(|f| f.name().explicit_pg_predicate().trgm_similar_to("Alice", 0.2))
    .annotate("name_score", |f| f.name().trgm_similarity("Alice"))
    .order_by_annotation("name_score", Ordering::Desc)
    .fetch_all(&mut ctx)
    .await?;
```

---

## Declaring a GIN or GiST index

Without an index, every `similarity()` call scans all rows. At scale (tens of
thousands of rows or more), declare a trgm-accelerated index on the column.

`pg_trgm` supports two index methods:

| Method | Opclass | Best for |
|---|---|---|
| `GIN` | `gin_trgm_ops` | Equality predicates (`trgm_similar_to`); high write throughput |
| `GiST` | `gist_trgm_ops` | Ordered similarity queries (`ORDER BY trgm_similarity`); lower build cost |

Use `IndexSpec` in your model's `#[model(indexes = [...])]` attribute:

```rust
use djogi::prelude::*;
use djogi::descriptor::{
    IndexColumnSpec, IndexKind, IndexNullsOrder, IndexOrder,
    IndexSpec, IndexTarget, IndexType,
};

#[model(
    app = "profiles",
    table = "user_profile",
    indexes = [
        // GIN index — accelerates WHERE similarity(bio, $1) >= $2 lookups.
        IndexSpec {
            name: "user_profile_bio_trgm_gin_idx",
            target: IndexTarget::Columns(&[IndexColumnSpec {
                name: "bio",
                opclass: Some("gin_trgm_ops"),
                order: IndexOrder::Asc,
                nulls: IndexNullsOrder::Default,
            }]),
            kind: IndexKind::NonUnique,
            index_type: IndexType::Gin,
            predicate: None,
            include: &[],
            nulls_not_distinct: false,
            requires_out_of_transaction: false,
            extension_dependency: Some("pg_trgm"),
        },
    ]
)]
pub struct UserProfile {
    pub name: String,
    pub bio: String,
}
```

The `extension_dependency: Some("pg_trgm")` field tells Djogi's migration
runner to emit `CREATE EXTENSION IF NOT EXISTS pg_trgm` before the index DDL
and to surface a clear error when the extension is absent.

### GiST variant

Replace `IndexType::Gin` / `"gin_trgm_ops"` with `IndexType::Gist` /
`"gist_trgm_ops"` for the GiST form:

```rust
IndexSpec {
    name: "user_profile_name_trgm_gist_idx",
    target: IndexTarget::Columns(&[IndexColumnSpec {
        name: "name",
        opclass: Some("gist_trgm_ops"),
        order: IndexOrder::Asc,
        nulls: IndexNullsOrder::Default,
    }]),
    kind: IndexKind::NonUnique,
    index_type: IndexType::Gist,
    predicate: None,
    include: &[],
    nulls_not_distinct: false,
    requires_out_of_transaction: false,
    extension_dependency: Some("pg_trgm"),
}
```

### Generated migration DDL

Djogi's migration emitter produces:

```sql
CREATE EXTENSION IF NOT EXISTS "pg_trgm";

CREATE INDEX user_profile_bio_trgm_gin_idx
    ON user_profile
    USING GIN (bio gin_trgm_ops);
```

---

## Testing with pg_trgm

Use `#[djogi_test(extensions = ["pg_trgm"])]` to auto-provision the extension
before each test database:

```rust
use djogi::prelude::*;

#[model(app = "profiles_test", table = "trgm_test_profiles")]
pub struct Profile {
    pub name: String,
}

#[djogi::djogi_test(
    sync_models = [Profile],
    extensions = ["pg_trgm"],
)]
async fn trgm_similarity_filters_by_threshold(mut ctx: djogi::DjogiContext) {
    Profile::create(&mut ctx, Profile { name: "Alice".to_string(), ..Default::default() }).await?;
    Profile::create(&mut ctx, Profile { name: "Bob".to_string(), ..Default::default() }).await?;

    let results = Profile::objects()
        .filter(|f| f.name().explicit_pg_predicate().trgm_similar_to("Alce", 0.4))
        .fetch_all(&mut ctx)
        .await?;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "Alice");
    Ok(())
}
```

The `extensions = ["pg_trgm"]` key instructs `djogi_test` to run
`CREATE EXTENSION IF NOT EXISTS pg_trgm` against the test database before
table setup. Extension names follow the same validation rules as Postgres
identifiers: ASCII letters, digits, and underscores only, max 63 bytes.

---

## Choosing a threshold

The `similarity()` function returns `0.0` for completely dissimilar strings and
`1.0` for identical strings. Typical thresholds:

| Threshold | Typical use |
|---|---|
| `0.1–0.2` | Highly permissive fuzzy match; useful for typo tolerance |
| `0.3` | Good default for name / tag autocomplete |
| `0.4–0.6` | Moderate strictness; bio / description partial match |
| `≥ 0.7` | Near-exact match; useful when you want "same word, different form" |

The right threshold is always corpus-dependent. Start with `0.3`, evaluate
precision/recall, and tune from there.

---

## API reference

### `ExplicitPgPredicateField<M, String>::trgm_similar_to`

Reached via `f.col().explicit_pg_predicate().trgm_similar_to(pattern, threshold)`.

```
pub fn trgm_similar_to(
    self,
    pattern: impl Into<String>,
    threshold: f64,
) -> Condition
```

Returns a `Condition` that evaluates `similarity(col, $pattern) >= $threshold`.
Both arguments are positional bind parameters. Postgres-specific — not
evaluable in Punnu's in-memory cache.

**Gate:** requires `djogi = { features = ["trgm"] }` and `pg_trgm` installed
in the target Postgres database.

**Index:** accelerated by a GIN index with `gin_trgm_ops` opclass.

---

### `DjogiField<M, String>::trgm_similarity`

Reached via `f.col().trgm_similarity(pattern)` directly in `filter_expr` /
`annotate` closures.

```
pub fn trgm_similarity(
    self,
    pattern: impl Into<String>,
) -> Expr<f64>
```

Returns an `Expr<f64>` evaluating `similarity(col, $pattern)` per row. The
result is in `[0.0, 1.0]`. Use in `filter_expr` to build threshold comparisons
(`expr.gte(Expr::literal(0.3_f64))`), or in `annotate` to surface the score as
a named computed column.

**Gate:** requires `djogi = { features = ["trgm"] }` and `pg_trgm` installed.

**Index:** a GiST index with `gist_trgm_ops` opclass accelerates similarity
lookups when the query includes an explicit `similarity()` comparison.
