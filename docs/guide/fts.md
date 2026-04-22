> [Back to README](../../ReadMe.MD) | [All Guides](./index.md)

# Full-Text Search

Djogi ships typed full-text search (FTS) as a first-class feature, built on
Postgres's native `tsvector` / `tsquery` engine. A single model attribute
wires the generated column, GIN index, and query API — no raw SQL required
for the common path.

---

## Declaring an FTS column

Add `fts(source = "...", dictionary = "...")` to the `#[model]` attribute:

```rust
use djogi::prelude::*;

#[model(
    table = "book",
    fts(source = "title, body", dictionary = "english"),
)]
pub struct Book {
    pub title: String,
    pub body: String,
}
```

Two keys are required:

| Key | Value | Example |
|---|---|---|
| `source` | Comma-separated list of column names to index | `"title, body"` |
| `dictionary` | Postgres text-search configuration name | `"english"` |

The macro emits an `FtsDescriptor` into `ModelDescriptor::fts` and synthesises
a `BookFields::search()` accessor. It does **not** add a `search` field to the
Rust struct — the tsvector column is database-managed.

### The generated migration DDL

Phase 6's migration differ will emit the following DDL for the model above:

```sql
ALTER TABLE book
  ADD COLUMN search TSVECTOR
    GENERATED ALWAYS AS (
      to_tsvector('english', title || ' ' || body)
    ) STORED;

CREATE INDEX book_search_gin ON book USING GIN (search);
```

Until Phase 6 ships, hand-apply the DDL in your migration files. The fixture
at `tests/integration/migrations/phase5/011_fts_book.sql` shows the canonical
shape. Use `ctx.raw_ddl(SQL)` (not `ctx.raw_execute`) to apply multi-statement
migration files.

---

## Querying — `.matches()`

`.matches(query)` builds a `Condition` leaf that emits `search @@ to_tsquery(...)`.
Use it inside a `.filter()` closure:

```rust
use djogi::prelude::*;

let hits = Book::objects()
    .filter(|b| b.search().matches(TsQuery::new("planet & earth")))
    .fetch_all(&mut ctx)
    .await?;
```

Generated SQL:

```sql
SELECT id, created_at, updated_at, title, body
FROM book
WHERE search @@ to_tsquery('english', $1)
```

`TsQuery::new(s)` wraps a raw tsquery operator string. Postgres parses it
server-side — errors from malformed query strings come back as `DjogiError::Db`,
not a compile error.

### tsquery operator syntax

| Syntax | Meaning | Example |
|---|---|---|
| `planet & earth` | AND — both must appear | All documents containing both "planet" and "earth" |
| `planet \| mars` | OR — at least one must appear | Documents about either planet |
| `!earth` | NOT — must not appear | Documents without "earth" |
| `'planet earth'` | Phrase query (PG 9.6+) | Adjacent terms in order |

---

## Ranking — `.rank()` and `.rank_cd()`

`.rank(query)` builds an `Expr<f32>` that emits `ts_rank(search, to_tsquery(...))`.
Use it in `.order_by()` to surface the most relevant results first:

```rust
let hits = Book::objects()
    .filter(|b| b.search().matches(TsQuery::new("planet & earth")))
    .order_by(|b| {
        b.search().rank(TsQuery::new("planet & earth")).desc()
    })
    .fetch_all(&mut ctx)
    .await?;
```

Generated SQL:

```sql
SELECT ...
FROM book
WHERE search @@ to_tsquery('english', $1)
ORDER BY ts_rank(search, to_tsquery('english', $1)) DESC
```

`.rank_cd(query)` uses `ts_rank_cd` (cover-density ranking), which weighs
term proximity more heavily. Use it when positional clustering of terms is
a stronger relevance signal than raw term frequency.

---

## Multi-column source

List all columns that contribute to search relevance in `source`:

```rust
#[model(
    table = "article",
    fts(source = "title, subtitle, body, tags", dictionary = "english"),
)]
pub struct Article {
    pub title: String,
    pub subtitle: String,
    pub body: String,
    pub tags: String,
}
```

Postgres concatenates the columns with a space separator before calling
`to_tsvector`. All listed columns contribute equally. For per-column weight
control (`setweight(to_tsvector(...), 'A')`) see the deferred note in
[Scope guardrails](#scope-guardrails).

---

## Dictionary selection

The `dictionary` key names any Postgres text-search configuration. The
built-in configurations are:

| Dictionary | Language | Notes |
|---|---|---|
| `english` | English | Snowball stemmer, English stop words |
| `spanish` | Spanish | Snowball stemmer, Spanish stop words |
| `french` | French | Snowball stemmer, French stop words |
| `german` | German | |
| `portuguese` | Portuguese | |
| `simple` | No stemming | Lowercasing only; useful for code/tags |

Full list: `SELECT cfgname FROM pg_ts_config;`

The dictionary name is validated at compile time (ASCII identifier, max 63
bytes). Runtime errors occur only if the named configuration does not exist
in the database — that is a deployment concern, not a code error.

---

## Changing the dictionary

Changing `dictionary` from one value to another (e.g. `"english"` to
`"spanish"`) is a **column-type alteration**. The GENERATED ALWAYS AS
expression embeds the dictionary name literally:

```sql
search TSVECTOR GENERATED ALWAYS AS (
    to_tsvector('english', title || ' ' || body)
) STORED
```

Altering it requires dropping and re-creating the generated column — a
full re-index operation. Phase 6's migration differ detects this case via
`FtsDescriptor` inequality and emits the appropriate `DROP COLUMN` /
`ADD COLUMN` DDL.

---

## GIN index

Djogi always emits a GIN index on the generated `search` column. GIN indexes
are the right choice for `@@` queries on static or infrequently-updated
tsvector data: they are slower to write than GiST but faster to query.

The index is named `{table}_search_gin` by convention. Phase 6's migration
differ creates it alongside the column.

---

## FTS in raw SQL

When `QuerySet` cannot express a query — e.g., multi-table FTS across a
JOIN, or using `plainto_tsquery` instead of `to_tsquery` — fall back to
raw SQL via `ctx.raw_query` / `ctx.__query_all_for_macros`:

```rust
let rows = ctx
    .__query_all_for_macros(
        "SELECT b.id, b.title, ts_rank(b.search, q) AS score \
         FROM book b, to_tsquery('english', $1) q \
         WHERE b.search @@ q \
         ORDER BY score DESC",
        &[&"planet & earth" as &(dyn ToSql + Sync)],
    )
    .await?;
```

The `search` column is a real Postgres column — it works in any SQL context.

---

## Scope guardrails

The following capabilities are **not yet implemented** and are deferred to
later phases:

- **Per-column weights** — `setweight(to_tsvector(...), 'A')` for title vs.
  body. Planned for Phase 8.
- **Custom generated column name** — always `"search"` today. A
  `column = "..."` override in `fts(...)` lands in Phase 8.
- **Migration differ wiring** — Phase 6 consumes `FtsDescriptor` to emit
  `ALTER TABLE` DDL. Until then, apply the DDL by hand.
- **`plainto_tsquery` / `phraseto_tsquery` builders** — only `to_tsquery`
  is surfaced today. Use raw SQL for the other query-construction functions.

---

## TsVector and TsQuery types

`TsVector` and `TsQuery` are exported from `djogi::prelude::*`:

- **`TsVector`** — a `TSVECTOR` column value decoded from Postgres. Appears
  in `FromPgRow` when you explicitly SELECT the `search` column.
- **`TsQuery`** — a query string you supply at call time:
  `TsQuery::new("planet & earth")`.

Both implement `postgres_types::{ToSql, FromSql}` against the `tsvector` /
`tsquery` wire types (with a TEXT fallback for older `postgres-types` builds).
