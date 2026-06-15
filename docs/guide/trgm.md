> [Back to README](../../README.md) | [All Guides](./index.md)

# Trigram Similarity (pg_trgm)

The `trgm` feature exposes Postgres's `pg_trgm` extension as a first-class
typed surface on text columns. Two operations are provided:

- `.explicit_pg_predicate().trgm_similar_to(pattern)` — a `WHERE`-clause
  predicate that compiles to the `%` operator. Index-accelerated by
  `gin_trgm_ops` / `gist_trgm_ops`. Threshold is the session GUC
  `pg_trgm.similarity_threshold` (Postgres default `0.3`). Postgres-specific,
  not portable to Punnu.
- `.trgm_similarity(pattern)` — a scored `Expr<f64>` for per-row similarity.
  Compose with the `Expr<T>` comparison API in `filter_expr` to apply a
  per-query numeric threshold. **Not** index-accelerated by the trgm
  opclasses — use this when the explicit numeric threshold matters more
  than peak read performance.

Together these cover the principal pg_trgm use-case: partial-match search
over user-visible strings (profile bios, tags, autocomplete, name lookups).
Unlike `ILIKE`, trigram similarity is language-agnostic and performs
acceptably without prefix or suffix anchoring.

---

## Enabling the feature

Add `trgm` to your `djogi` dependency in `Cargo.toml`:

```toml
[dependencies]
djogi = { version = "...", features = ["trgm"] }
```

**Extension requirement:** `pg_trgm` must be installed in the target Postgres
database before any query using the `%` operator or the `similarity()`
function can execute:

```sql
CREATE EXTENSION IF NOT EXISTS pg_trgm;
```

Djogi's migration runner installs `pg_trgm` automatically — see
[Generated migration DDL](#generated-migration-ddl) below for the two-file
split (Phase 0 bootstrap migration for the extension; per-app migration for
the index). If your application role does not have `CREATE EXTENSION`
privileges, a database administrator must install `pg_trgm` out of band.

---

## Predicate — `trgm_similar_to`

```rust
ExplicitPgPredicateField<M, String>::trgm_similar_to(
    pattern: impl Into<String>,
) -> Condition
```

Filters rows where the column value is trigram-similar to `pattern` under
Postgres's `%` operator. The `%` operator is the indexable strategy member
of the `gin_trgm_ops` and `gist_trgm_ops` opclasses — declare a GIN or GiST
index with one of those opclasses and the predicate is accelerated by the
index.

The threshold for `%` is the **session GUC** `pg_trgm.similarity_threshold`
(Postgres default `0.3`). Adjust it per session or per transaction with
`SET` / `SET LOCAL`:

```sql
-- Session-wide override:
SET pg_trgm.similarity_threshold = 0.4;

-- Per-transaction override:
BEGIN;
SET LOCAL pg_trgm.similarity_threshold = 0.4;
-- queries here see 0.4
COMMIT;
```

For a per-query numeric threshold without touching the GUC, use the
`trgm_similarity` expression form below — at the cost of giving up the
index-acceleration affordance.

`trgm_similar_to` is a Postgres-specific predicate (it requires the
`pg_trgm` extension and is not evaluable in Punnu's in-memory cache). Reach
it via `.explicit_pg_predicate()` — the same pattern as `regex` / `iregex`:

```rust
use djogi::prelude::*;

#[model(app = "profiles", table = "user_profile")]
pub struct UserProfile {
    pub name: String,
    pub bio: String,
}

// Find profiles whose bio is trigram-similar to "machine learning"
// at the session's current pg_trgm.similarity_threshold.
let matches = UserProfile::objects()
    .filter(|f| f.bio().explicit_pg_predicate().trgm_similar_to("machine learning"))
    .fetch_all(&mut ctx)
    .await?;
```

Generated SQL:

```sql
SELECT id, created_at, updated_at, name, bio
FROM user_profile
WHERE bio % $1
-- binds: $1 = "machine learning"
```

The `pattern` is a positional bind parameter — no user text is ever
interpolated into SQL.

---

## Per-query numeric threshold — `trgm_similarity` + `filter_expr`

When you need a specific numeric threshold applied at the query site (not a
session-wide GUC change), compose `trgm_similarity` with the typed `Expr<T>`
comparison API inside `filter_expr`:

```rust
DjogiField<M, String>::trgm_similarity(
    pattern: impl Into<String>,
) -> Expr<f64>
```

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

**Index acceleration note:** this expression form is **NOT** index-
accelerated by `gin_trgm_ops` / `gist_trgm_ops`. The `%` operator is the
indexable strategy member; `similarity(col, x) >= threshold` is a
function-form comparison that Postgres evaluates per row (sequential
scan, or index-only scan if the planner can find an unrelated covering
index). Use `filter_expr` when the numeric threshold matters more than
peak performance — or pair it with the `%` operator to get an index seek
first followed by a tighter threshold:

```rust
let matches = UserProfile::objects()
    // First conjunct: indexable, narrows via gin_trgm_ops.
    .filter(|f| {
        f.bio()
            .explicit_pg_predicate()
            .trgm_similar_to("machine learning")
    })
    // Second conjunct: tightens the threshold for the matched candidates.
    .filter_expr(|f| {
        f.bio()
            .trgm_similarity("machine learning")
            .gte(Expr::literal(0.5_f64))
    })
    .fetch_all(&mut ctx)
    .await?;
```

This pairing is only meaningful when the tighter threshold is **above**
the session GUC — otherwise the `%` operator already filters at the lower
GUC threshold and the `filter_expr` conjunct is redundant.

---

## Declaring a GIN or GiST index

Without a trgm-opclass index, the `%` operator falls back to a sequential
scan that recomputes trigrams per row. At scale (tens of thousands of rows
or more), declare a trgm-accelerated index on the column.

`pg_trgm` supports two index methods:

| Method | Opclass | Best for |
|---|---|---|
| `GIN` | `gin_trgm_ops` | High-throughput `%` operator scans; faster reads, slower writes |
| `GiST` | `gist_trgm_ops` | Same `%` scans plus distance ordering with `<->` (not yet at the typed surface); balanced read/write cost |

**Index coverage note:** both opclasses accelerate the `%`, `<%`, `<<%`,
`<->`, `<<->`, `<<<->`, and `=` operators (per Postgres
[F.35.4 Index Support](https://www.postgresql.org/docs/18/pgtrgm.html#PGTRGM-INDEX)).
They do **not** accelerate the function-form predicate
`similarity(col, x) >= y` — that comparison is what
`trgm_similarity(...).gte(...)` emits and it falls back to a per-row
function call. djogi's `trgm_similar_to` compiles to `%` precisely so this
index acceleration is realized.

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
        // GIN index — accelerates the `%` operator emitted by trgm_similar_to.
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
runner that this index requires `pg_trgm`; the runner ensures
`CREATE EXTENSION IF NOT EXISTS "pg_trgm"` lands in the Phase 0 bootstrap
migration for this database before any `CREATE INDEX` that depends on it
runs.

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

A trgm-using app produces **two** distinct migration files on the first
compose:

1. **`migrations/<database>/V00000000000000__phase_zero_bootstrap.sdjql`** — the Phase 0 bootstrap
   migration. Djogi auto-emits this file (and re-emits when the descriptor's
   extension dependencies change) so adopters never hand-author the
   extension install. It contains the HeeRanjID schema plus every
   `CREATE EXTENSION` derived from `extension_dependency` across the
   descriptor inventory:

   ```sql
   -- Postgres extensions required by descriptor inventory (idempotent).
   CREATE EXTENSION IF NOT EXISTS "pg_trgm";
   ```

   See `djogi/src/migrate/bootstrap.rs` for the composition logic and
   `migrations/<database>/V00000000000000__phase_zero_bootstrap.sdjql` in your repo for the
   committed file.

2. **`migrations/<database>/V<ts>__slug.sdjql`** — the per-app migration
   that introduces the trgm index. The emitter renders index DDL with
   lowercase method name and quoted identifiers (compatible with the
   uppercase / unquoted form Postgres accepts; the quoting is structural
   belt-and-braces against future spec changes):

   ```sql
   CREATE INDEX "user_profile_bio_trgm_gin_idx"
       ON "user_profile"
       USING gin ("bio" gin_trgm_ops);
   ```

Both files apply transactionally per migration; the Phase 0 bootstrap runs
first, then the per-app migration that references the extension.

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
async fn trgm_similar_to_at_session_threshold(mut ctx: djogi::DjogiContext) {
    Profile::create(&mut ctx, Profile { name: "Alice".to_string(), ..Default::default() })
        .await
        .expect("create Alice must succeed");
    Profile::create(&mut ctx, Profile { name: "Bob".to_string(), ..Default::default() })
        .await
        .expect("create Bob must succeed");

    // The default pg_trgm.similarity_threshold is 0.3 — "Alce" matches "Alice".
    let results = Profile::objects()
        .filter(|f| f.name().explicit_pg_predicate().trgm_similar_to("Alce"))
        .fetch_all(&mut ctx)
        .await
        .expect("trgm_similar_to fetch must succeed");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "Alice");
}
```

The `extensions = ["pg_trgm"]` key instructs `djogi_test` to run
`CREATE EXTENSION IF NOT EXISTS pg_trgm` against the test database before
table setup. Extension names follow the same validation rules as Postgres
identifiers: ASCII letters, digits, and underscores only, max 63 bytes.

---

## Choosing a threshold

The session GUC `pg_trgm.similarity_threshold` controls how strict the `%`
operator is. Postgres defaults to `0.3`. Typical values:

| Threshold | Typical use |
|---|---|
| `0.1–0.2` | Highly permissive fuzzy match; useful for typo tolerance |
| `0.3` | Postgres default — good for name / tag autocomplete |
| `0.4–0.6` | Moderate strictness; bio / description partial match |
| `≥ 0.7` | Near-exact match; useful when you want "same word, different form" |

For a session-wide change use `SET pg_trgm.similarity_threshold = 0.4`. For
a single transaction use `SET LOCAL pg_trgm.similarity_threshold = 0.4`
inside a `BEGIN`/`COMMIT` block. For a single query, prefer the
`filter_expr` form above — at the cost of giving up index acceleration on
that conjunct.

The right threshold is always corpus-dependent. Start with `0.3`, evaluate
precision/recall, and tune from there.

---

## Limitations and future work

**Ranked retrieval is not yet wired through the typed surface.** The
canonical pg_trgm ranked-result query is:

```sql
SELECT t, similarity(t, 'word') AS sml
FROM test_trgm
WHERE t % 'word'
ORDER BY sml DESC, t;
```

In v0.1.0, the `WHERE t % 'word'` half is expressed by `trgm_similar_to`,
but the `ORDER BY similarity(t, 'word') DESC` half and surfacing the score
as a named column via `annotate(...)` are framework gaps — they require an
`OrderExpr` variant carrying a generic `Expr<T>` and an `AnnotationSlot`
impl for `Expr<V>` that do not exist yet. The same gap affects
`TsRank` / `TsRankCd` in the FTS feature and any future score-producing
expression. Until that follow-up lands, ranked retrieval has to go through
one of:

- The two-step shape — `filter(...trgm_similar_to(...))` to narrow via
  index, then `filter_expr(...trgm_similarity(...).gte(...))` to tighten,
  then app-side sorting on a separate score column fetched independently.
- An adopter-side raw-SQL helper. djogi treats raw SQL like Rust's
  `unsafe`: legitimate when typed surface gaps exist, audited via the
  `#[djogi::deliberately_bypass_convention_with_raw_sql]` attribute and a
  `// JUSTIFICATION (djogi#<issue>):` comment that names the
  follow-up. See
  [`docs/spec/raw-sql-escape-hatches.md`](../spec/raw-sql-escape-hatches.md)
  for the convention.

A follow-up issue tracks the `OrderExpr` / `AnnotationSlot` integration
for generic `Expr<T>`; once it lands, this section will be replaced with
the working ranked-retrieval shape.

**Threshold validation is not performed at the typed surface.**
`f64::NAN`, infinities, and values outside `[0.0, 1.0]` reach Postgres
without diagnostic and produce empty / universal result sets. Callers
that build thresholds from untrusted input should validate before
calling `.gte(Expr::literal(threshold))`.

---

## API reference

### `ExplicitPgPredicateField<M, String>::trgm_similar_to`

Reached via `f.col().explicit_pg_predicate().trgm_similar_to(pattern)`.

```text
pub fn trgm_similar_to(
    self,
    pattern: impl Into<String>,
) -> Condition
```

Returns a `Condition` that evaluates `<col> % $pattern`. The pattern is a
positional bind parameter. Postgres-specific — not evaluable in Punnu's
in-memory cache.

**Threshold:** controlled by the session GUC `pg_trgm.similarity_threshold`
(default `0.3`). Use `SET` / `SET LOCAL` to override per session or per
transaction. For a per-query numeric threshold use the
`trgm_similarity` expression form below.

**Gate:** requires `djogi = { features = ["trgm"] }` and `pg_trgm`
installed in the target Postgres database.

**Index:** index-accelerated by a GIN index with `gin_trgm_ops` or a
GiST index with `gist_trgm_ops`. Without the index, the `%` operator
falls back to a sequential scan.

---

### `DjogiField<M, String>::trgm_similarity`

Reached via `f.col().trgm_similarity(pattern)` directly in `filter_expr`
closures.

```text
pub fn trgm_similarity(
    self,
    pattern: impl Into<String>,
) -> Expr<f64>
```

Returns an `Expr<f64>` evaluating `similarity(col, $pattern)` per row. The
result is in `[0.0, 1.0]`. Use in `filter_expr` to build per-query
numeric-threshold comparisons (`expr.gte(Expr::literal(0.3_f64))`).

**Gate:** requires `djogi = { features = ["trgm"] }` and `pg_trgm`
installed.

**Index:** NOT accelerated by `gin_trgm_ops` / `gist_trgm_ops` (these
opclasses target operators, not the function form). For index-accelerated
trgm scans, use `trgm_similar_to` above.

**Future work:** `Expr<f64>` cannot yet be used as an `order_by` target
or as an `annotate` payload — both require generic-`Expr` integration on
`OrderExpr` and `AnnotationSlot`. See the
[Limitations](#limitations-and-future-work) section above.
