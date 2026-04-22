# sea-query

## Metadata

- **Clone path:** `/home/tarunvir/projects/seaquery-reference/`
- **Commit SHA:** `018efe989b842ea6b067eeae952dd82b81b4560b`
- **Primary language:** Rust
- **Version:** `1.0.0-rc.33` (`Cargo.toml:6`)
- **Total LOC (src/):** ~32,300 lines across all source files (19,741 in the main modules, rest in value/extension sub-modules)
- **Workspace members:** `sea-query` (main crate) + `sea-query-derive` (proc macro sub-crate) (`Cargo.toml:1-2`)

---

## Architecture

### Crate structure

Single workspace with two crates: the main `sea-query` library and `sea-query-derive` for the `#[derive(Iden)]` / `#[enum_def]` proc macros (`Cargo.toml:1-2`). The integration adapters (`sea-query-postgres`, `sea-query-sqlx`, `sea-query-rusqlite`, `sea-query-rbatis`, `sea-query-diesel`) live as separate sibling directories in the repo but are not in the workspace members — they are standalone companion crates.

### Main modules (`src/lib.rs:974-1012`)

| Module | Role |
|---|---|
| `src/table/` | `Table::create`, `Table::alter`, `Table::drop`, `Table::rename`, `Table::truncate` |
| `src/index/` | `Index::create`, `Index::drop` |
| `src/foreign_key/` | `ForeignKey::create`, `ForeignKey::drop` |
| `src/backend/postgres/` | Postgres-specific DDL rendering |
| `src/backend/` | `SchemaBuilder`, `TableBuilder`, `IndexBuilder`, `ForeignKeyBuilder` traits |
| `src/expr/` | `Expr`, `ExprTrait` — expressions for WHERE, DEFAULT, CHECK, etc. |
| `src/types/` | `Iden`, `DynIden`, `IntoIden`, column types, identifiers |
| `src/value/` | `Value` enum, type-extension modules (`with-chrono`, `with-uuid`, etc.) |
| `src/extension/postgres/` | Postgres-specific extensions: JSON functions, ltree, explain, types |

### Key entry points for DDL

- `Table::create()` → `TableCreateStatement` (`src/table/create.rs:82-95`)
- `Table::alter()` → `TableAlterStatement` (`src/table/alter.rs:32-36`)
- `Index::create()` → `IndexCreateStatement` (`src/index/create.rs:211-222`)
- `ForeignKey::create()` → `ForeignKeyCreateStatement` (`src/foreign_key/create.rs`)
- All DDL statements implement `SchemaStatementBuilder` with `.build(PostgresQueryBuilder)` → `String` (`src/backend/mod.rs:37`)

---

## State model (source-of-truth)

N/A — sea-query is a builder; state is held by the consumer (e.g., SeaORM's migration runner). The in-memory statement AST (e.g., `TableCreateStatement`, `IndexCreateStatement`) is the "state" sea-query exposes. sea-query has no concept of applied vs pending migrations.

---

## Ledger / history table

N/A — sea-query is a query builder, not a runner. No ledger, no history table.

---

## Execution

N/A — sea-query emits SQL strings; it does not execute them. The consumer decides transaction boundaries and execution order. From `SchemaStatementBuilder`:

```rust
fn build<T: SchemaBuilder>(&self, schema_builder: T) -> String
```

(`src/lib.rs:486-489`). The call site receives a plain `String` and passes it to whatever executor it chooses.

---

## Recovery

N/A — sea-query has no recovery, checksums, or repair.

---

## Diff and generation

### DDL generation surface (confidence: high)

sea-query can generate all of the following DDL:

| Statement | struct | source |
|---|---|---|
| `CREATE TABLE` | `TableCreateStatement` | `src/table/create.rs:82` |
| `ALTER TABLE ADD COLUMN` | `TableAlterStatement` + `TableAlterOption::AddColumn` | `src/table/alter.rs:56` |
| `ALTER TABLE ADD COLUMN IF NOT EXISTS` | same with `if_not_exists: true` | `src/table/alter.rs:40-43` |
| `ALTER TABLE MODIFY COLUMN` (Postgres: `ALTER COLUMN TYPE`) | `TableAlterOption::ModifyColumn` | `src/table/alter.rs:58` |
| `ALTER TABLE RENAME COLUMN` | `TableAlterOption::RenameColumn` | `src/table/alter.rs:59` |
| `ALTER TABLE DROP COLUMN` | `TableAlterOption::DropColumn` | `src/table/alter.rs:60` |
| `ALTER TABLE DROP COLUMN IF EXISTS` | same with `if_exists: true` | `src/table/alter.rs:46-50` |
| `ALTER TABLE ADD CONSTRAINT FOREIGN KEY` | `TableAlterOption::AddForeignKey` | `src/table/alter.rs:61` |
| `ALTER TABLE DROP CONSTRAINT` | `TableAlterOption::DropConstraint` | `src/table/alter.rs:63` |
| `DROP TABLE` | `TableDropStatement` | `src/table/drop.rs` |
| `RENAME TABLE` / `ALTER TABLE ... RENAME TO` | `TableRenameStatement` | `src/table/rename.rs` |
| `TRUNCATE TABLE` | `TableTruncateStatement` | `src/table/truncate.rs` |
| `CREATE INDEX` | `IndexCreateStatement` | `src/index/create.rs:211` |
| `DROP INDEX` | `IndexDropStatement` | `src/index/drop.rs` |
| `ALTER TABLE ADD CONSTRAINT FOREIGN KEY` (standalone) | `ForeignKeyCreateStatement` | `src/foreign_key/create.rs` |
| `ALTER TABLE DROP CONSTRAINT` (FK) | `ForeignKeyDropStatement` | `src/foreign_key/drop.rs` |
| `CREATE TYPE ... AS ENUM` | `TypeCreateStatement` | `src/backend/postgres/types.rs:5` |
| `DROP TYPE` | `TypeDropStatement` | `src/backend/postgres/types.rs:40` |
| `ALTER TYPE ADD VALUE / RENAME` | `TypeAlterStatement` | `src/backend/postgres/types.rs:66` |

### Postgres dialect features (confidence: high)

Checked against `src/backend/postgres/`:

| Feature | Supported | Source |
|---|---|---|
| `CREATE INDEX CONCURRENTLY` | Yes — `.concurrently()` flag on `IndexCreateStatement` | `src/index/create.rs:216`, `src/backend/postgres/index.rs:42-44` |
| `DROP INDEX CONCURRENTLY` | Yes — same flag on `IndexDropStatement` | `src/backend/postgres/index.rs:91-93` |
| Partial indexes (`WHERE` predicate) | Yes — `IndexCreateStatement` implements `ConditionalStatement`; `.and_where(expr)` | `src/index/create.rs:377-390`, `src/backend/postgres/index.rs:73` |
| Expression (functional) indexes | Yes — `Expr` can be passed to `.col()` on `IndexCreateStatement` | `src/index/common.rs:161-178`, test `tests/postgres/index.rs:132-141` |
| `INCLUDE` columns | Yes — `.include(col)` on `IndexCreateStatement` | `src/index/create.rs:321-326`, `src/backend/postgres/index.rs:65-69` |
| `NULLS NOT DISTINCT` | Yes — `.nulls_not_distinct()` flag | `src/index/create.rs:217`, `src/backend/postgres/index.rs:21-23` |
| `USING GIN / BTREE / HASH` | Yes — `IndexType` enum with `Custom(DynIden)` fallback | `src/index/create.rs:226-232`, `src/backend/postgres/index.rs:121-132` |
| `USING GIST` | Via `IndexType::Custom("gist")` — not a named variant | `src/index/create.rs:231` |
| Operator classes (e.g., `text_pattern_ops`) | Yes — `.with_operator_class(...)` on `IndexColumn` | `src/index/common.rs:51-62`, test `tests/postgres/index.rs:161-175` |
| `jsonb` column type | Yes — `ColumnType::JsonBinary` → `"jsonb"` | `src/backend/postgres/table.rs:95` |
| `json` column type | Yes — `ColumnType::Json` → `"json"` | `src/backend/postgres/table.rs:94` |
| `uuid` | Yes | `src/backend/postgres/table.rs:96` |
| Arrays (`text[]`, etc.) | Yes — `ColumnType::Array(elem_type)` | `src/backend/postgres/table.rs:97-100` |
| `vector(N)` (pgvector) | Yes — `ColumnType::Vector(size)`, feature `postgres-vector` | `src/backend/postgres/table.rs:101-107` |
| `cidr`, `inet`, `macaddr` | Yes | `src/backend/postgres/table.rs:111-114` |
| `ltree` | Yes | `src/backend/postgres/table.rs:115` |
| `interval` with fields and precision | Yes — `ColumnType::Interval(Option<PgInterval>, Option<u32>)` | `src/backend/postgres/table.rs:53-66`, `src/table/column.rs:99` |
| Generated columns (`GENERATED ALWAYS AS`) | Yes — `.generated(expr, stored)` on `ColumnDef` | `src/table/column.rs:825-834`, test doc at line 812 |
| `GENERATED BY DEFAULT AS IDENTITY` (auto-increment) | Yes — default auto-increment strategy for Postgres | `src/backend/postgres/table.rs:241-246` |
| `SERIAL` columns | Yes — opt-in via feature `option-postgres-use-serial` | `src/backend/postgres/table.rs:233-238` |
| Column-level CHECK constraints | Yes — `.check(expr)` or `.check(("name", expr))` on `ColumnDef` | `src/table/column.rs:786-793`, `src/table/constraint.rs:1-49` |
| Table-level CHECK constraints | Yes — `.check(expr)` on `TableCreateStatement` | `src/table/create.rs:149-155`, rendered in `src/backend/table_builder.rs:51-57` |
| Named CHECK constraints | Yes — `Check::named(name, expr)` | `src/table/constraint.rs:11-19` |
| `ALTER TABLE DROP CONSTRAINT` | Yes | `src/table/alter.rs:419-424` |
| `ALTER TYPE ADD VALUE IF NOT EXISTS` | Yes | `src/backend/postgres/types.rs:95-118` |
| `CREATE TEMPORARY TABLE` | Yes — `.temporary()` on `TableCreateStatement` | `src/table/create.rs:405-407` |
| Exclusion constraints (`EXCLUDE USING`) | **No** — not represented in any struct or builder |
| `DEFERRABLE` / `INITIALLY DEFERRED` FK/constraint | **No** — not in `TableForeignKey` or any constraint struct |
| `ALTER TABLE ... NOT VALID` / `VALIDATE CONSTRAINT` | **No** — not present in `TableAlterOption` enum |
| `NULLS FIRST / NULLS LAST` on index columns | **No** — `IndexOrder` only has `Asc` and `Desc` (`src/index/common.rs:67-70`) |

**NULLS FIRST/LAST note:** `NullOrdering` with `First`/`Last` variants exists in the query builder (ORDER BY in SELECT), at `src/query/ordered.rs:3`, but is not wired into `IndexOrder` for index column definitions. There is no way to emit `("col" ASC NULLS LAST)` in a `CREATE INDEX` via sea-query.

### Verbatim SQL spot-check: composite unique index with ordering

Input Rust (from `src/index/create.rs:104-128`, doc-test):

```rust
let index = Index::create()
    .name("idx-glyph-aspect")
    .table(Glyph::Table)
    .col((Glyph::Image, IndexOrder::Asc))
    .col((Glyph::Aspect, IndexOrder::Desc))
    .unique()
    .to_owned();
```

Output SQL (Postgres):

```sql
CREATE UNIQUE INDEX "idx-glyph-aspect" ON "glyph" ("image" ASC, "aspect" DESC)
```

(`src/index/create.rs:123-127` — verbatim from doc-test assert)

### Verbatim SQL spot-check: partial index with WHERE

Input Rust (from `src/index/create.rs:155-172`, doc-test):

```rust
let index = Index::create()
    .name("idx-glyph-aspect")
    .table(Glyph::Table)
    .col((Glyph::Aspect, 64, IndexOrder::Asc))
    .and_where(Expr::col((Glyph::Table, Glyph::Aspect)).is_in(vec![3, 4]))
    .to_owned();
```

Output SQL (Postgres):

```sql
CREATE INDEX "idx-glyph-aspect" ON "glyph" ("aspect" (64) ASC) WHERE "glyph"."aspect" IN (3, 4)
```

(`src/index/create.rs:165-167` — verbatim from doc-test assert)

### Verbatim SQL spot-check: CREATE INDEX CONCURRENTLY

From `tests/postgres/index.rs:43-54`:

```rust
Index::create()
    .full_text()
    .name("idx-glyph-image")
    .concurrently()
    .table(Glyph::Table)
    .col(Glyph::Image)
    .to_string(PostgresQueryBuilder)
```

Output:

```sql
CREATE INDEX CONCURRENTLY "idx-glyph-image" ON "glyph" USING GIN ("image")
```

### Verbatim SQL spot-check: inline composite unique constraint in CREATE TABLE

From `tests/postgres/table.rs:310-336`:

```rust
Table::create()
    .table(Glyph::Table)
    .col(ColumnDef::new(Glyph::Image).json())
    .col(ColumnDef::new(Glyph::Aspect).json_binary())
    .index(
        Index::create()
            .unique()
            .nulls_not_distinct()
            .name("idx-glyph-aspect-image")
            .table(Glyph::Table)
            .col(Glyph::Aspect)
            .col(Glyph::Image)
    )
    .to_string(PostgresQueryBuilder)
```

Output:

```sql
CREATE TABLE "glyph" ( "image" json, "aspect" jsonb, CONSTRAINT "idx-glyph-aspect-image" UNIQUE NULLS NOT DISTINCT ("aspect", "image") )
```

(`tests/postgres/table.rs:327-335` — verbatim from test assert)

---

## Schema metadata

### Composite unique constraints

Two mechanisms (confidence: high):

1. **Standalone `CREATE UNIQUE INDEX`** — `Index::create().unique().name("...").col(col1).col(col2)`. The user **must** supply the name; sea-query never auto-names indexes. (`src/index/create.rs:258-263`, `src/index/common.rs:188-193`)

2. **Inline in `CREATE TABLE`** via `.index(Index::create().unique().name("...").col(col1).col(col2))` — renders as `CONSTRAINT "name" UNIQUE (col1, col2)` using `prepare_table_index_expression` in the Postgres backend. (`src/backend/postgres/index.rs:6-31`, `tests/postgres/table.rs:315-335`)

**Naming convention: none built in.** sea-query requires the caller to supply an explicit string name. There is no auto-generation of names from table/column combinations. (`src/index/create.rs:258-263`: `fn name<T: Into<String>>(&mut self, name: T)`)

### Composite indexes

- Multiple `.col()` calls on `IndexCreateStatement` — each call appends one `IndexColumn` to `TableIndex.columns` (`src/index/common.rs:196-199`)
- Per-column `ASC` / `DESC` via `IndexOrder::Asc` / `IndexOrder::Desc` as a tuple element: `.col((col, IndexOrder::Desc))` (`src/index/common.rs:113-125`)
- `NULLS FIRST` / `NULLS LAST` **not supported** for index columns — `IndexOrder` only has `Asc`/`Desc` (`src/index/common.rs:67-70`). This is a gap vs. Postgres's full index ordering specification.
- Expression columns in indexes (functional indexes): pass `Expr` directly to `.col()` (`src/index/common.rs:161-169`)
- Operator classes: `.with_operator_class(iden)` on a built `IndexColumn` (`src/index/common.rs:51-62`)

### Foreign keys

`TableForeignKey` struct (`src/foreign_key/common.rs:5-13`):

- `name: Option<String>` — user-supplied name required to produce `CONSTRAINT "name" FOREIGN KEY`
- `columns: Vec<DynIden>` — multi-column FK supported
- `ref_columns: Vec<DynIden>` — multi-column reference supported
- `on_delete: Option<ForeignKeyAction>` — variants: `Restrict`, `Cascade`, `SetNull`, `NoAction`, `SetDefault` (`src/foreign_key/common.rs:18-24`)
- `on_update: Option<ForeignKeyAction>` — same variants
- **No deferrability** — `DEFERRABLE INITIALLY DEFERRED` is not representable (confirmed: no field in struct)
- Postgres backend renders: `ALTER TABLE "t" ADD CONSTRAINT "name" FOREIGN KEY ("col") REFERENCES "ref" ("rcol") ON DELETE CASCADE` (`src/backend/postgres/foreign_key.rs:26-97`)

### CHECK constraints

Two levels (confidence: high):

1. **Column-level:** `.check(expr)` or `.check(("constraint_name", expr))` on `ColumnDef` — stored in `ColumnSpec.check: Option<Check>` (`src/table/column.rs:187-198`, `src/table/column.rs:786-793`)
2. **Table-level:** `.check(expr)` on `TableCreateStatement` — stored in `Vec<Check>` and rendered after foreign keys (`src/table/create.rs:149-155`, `src/backend/table_builder.rs:51-57`)

`Check` struct supports named and unnamed forms (`src/table/constraint.rs:4-49`). Named form renders as `CONSTRAINT "name" CHECK (expr)` (`src/backend/table_builder.rs:255-265`).

### Reflection / introspection

**None.** sea-query only emits SQL; it has no parser, no database introspection, and no facility to reconstruct a schema from a live database or DDL string. (Confirmed: no `parse`, `from_sql`, `introspect`, or `reflect` function anywhere in `src/`.) This is emit-only.

---

## Online-safe / staged migration guidance

### `CREATE INDEX CONCURRENTLY`

Fully supported as a first-class flag on `IndexCreateStatement`:

```rust
Index::create().concurrently().name("...").table("...").col("...")
```

Emits: `CREATE INDEX CONCURRENTLY "name" ON "table" ("col")` (`src/backend/postgres/index.rs:42-44`, `src/index/create.rs:297-300`)

`DROP INDEX CONCURRENTLY` is also supported on `IndexDropStatement` (`src/backend/postgres/index.rs:91-93`).

**Critical caveat:** sea-query emits the string — it does not enforce that `CREATE INDEX CONCURRENTLY` is run outside a transaction. That safety constraint is entirely the consumer's responsibility.

### `NOT VALID` / `VALIDATE CONSTRAINT` (two-phase constraint addition)

**Not supported.** `TableAlterOption` has no `NotValidConstraint` or `ValidateConstraint` variant (`src/table/alter.rs:56-64`). There is no way to emit `ALTER TABLE ... ADD CONSTRAINT ... NOT VALID` or `ALTER TABLE ... VALIDATE CONSTRAINT ...` through the typed API.

**Workaround:** `ColumnDef::extra(raw_string)` or `TableCreateStatement::extra(raw_string)` can inject arbitrary raw SQL fragments (`src/table/column.rs:866-872`, `src/table/create.rs:336-343`), but this bypasses all type safety.

---

## Rust-specific concerns

### Macro use

- `#[derive(Iden)]` — derive macro that implements `Iden` for enums and structs, converting `CamelCase` variants to `snake_case` identifier strings. Entry point: `sea-query-derive` sub-crate. (`src/lib.rs:1011-1012`, `Cargo.toml:32`)
- `#[enum_def]` — proc macro attribute that generates an `*Iden` enum from a struct definition (`src/lib.rs:1012`)
- `raw_query!` and `raw_sql!` macros — for ergonomic interpolated raw SQL with typed parameter binding and re-sequencing (`src/lib.rs:263-335`)
- No runtime proc macros; all macros are compile-time.

### Trait design

- `Iden` — the core identifier trait; one method `unquoted(&self) -> &str`. Implemented by enums, structs, and `&'static str`. (`src/types/iden/core.rs:5-38`)
- `DynIden` — an eagerly-rendered `Cow<'static, str>` wrapper replacing the older `Box<dyn Iden>` pattern. (`src/types/iden/core.rs:73-74`)
- `IntoIden` — blanket `From<T: Iden>` conversion for accepting anything Iden-like.
- `SchemaBuilder: TableBuilder + IndexBuilder + ForeignKeyBuilder` — the top-level DDL trait. `PostgresQueryBuilder` is the concrete implementor for Postgres. (`src/backend/mod.rs:37`)
- `SchemaStatementBuilder` — implemented by each statement struct; provides `.build(backend)` → `String`. (`src/lib.rs:484-489`)

### Feature flags (confidence: high, from `Cargo.toml:53-124`)

| Flag | Effect |
|---|---|
| `backend-postgres` | Enables `PostgresQueryBuilder` and all Postgres-specific DDL |
| `backend-mysql` | MySQL backend (irrelevant for Djogi) |
| `backend-sqlite` | SQLite backend (irrelevant for Djogi) |
| `derive` | `#[derive(Iden)]` proc macro |
| `with-chrono` | `chrono` date/time types in `Value` |
| `with-time` | `time` crate date/time types in `Value` |
| `with-json` | `serde_json::Value` in `Value` |
| `with-uuid` | `uuid::Uuid` in `Value` |
| `postgres-array` | `ColumnType::Array` for Postgres array columns |
| `postgres-vector` | `ColumnType::Vector` (pgvector) |
| `postgres-interval` | `ColumnType::Interval` |
| `postgres-range` | Postgres range types |
| `option-postgres-use-serial` | Use `SERIAL` instead of `GENERATED BY DEFAULT AS IDENTITY` |
| `thread-safe` | Makes `IdenStatic` require `Send + Sync` |

A Postgres-only Djogi consumer would need at minimum: `backend-postgres`, `with-json`, `with-uuid`, `with-time`, `postgres-array`, `postgres-vector` (if needed). No multi-dialect overhead at runtime — unused backends compile out via `#[cfg(feature)]`.

### Ergonomics of a long ALTER TABLE

`TableAlterStatement` supports chaining multiple options via a fluent `&mut Self` builder. All options are accumulated in `Vec<TableAlterOption>` and rendered as a comma-separated single `ALTER TABLE "t" op1, op2, op3` statement. (`src/table/alter.rs:426-429`, `src/backend/postgres/table.rs:134-212`)

Example from `src/table/alter.rs:280-340` (doc-test for `add_foreign_key`):

```rust
Table::alter()
    .table(Character::Table)
    .add_foreign_key(&foreign_key_char)
    .add_foreign_key(&foreign_key_font)
    .to_owned()
```

Output:

```sql
ALTER TABLE "character"
ADD CONSTRAINT "FK_character_glyph" FOREIGN KEY ("font_id", "id") REFERENCES "glyph" ("font_id", "id") ON DELETE CASCADE ON UPDATE CASCADE,
ADD CONSTRAINT "FK_character_font" FOREIGN KEY ("font_id") REFERENCES "font" ("id") ON DELETE CASCADE ON UPDATE CASCADE
```

The multi-op ALTER is readable but limited: you cannot mix `ADD COLUMN` and `ADD CONSTRAINT` with a `RENAME COLUMN` in a single statement by design (each is its own statement type). For complex migrations with multiple ALTER TABLE subcommands you accumulate multiple `TableAlterStatement` objects and emit them separately — the caller sequences them.

---

## Lessons for Djogi

### Adopt

- **`CREATE INDEX CONCURRENTLY` support** (citation: `src/index/create.rs:296-300`). sea-query's `.concurrently()` flag directly maps to Djogi's need for online-safe index creation. If Djogi uses sea-query for DDL emission, this comes for free. If Djogi formats strings directly, it should implement an equivalent boolean on its own index descriptor.

- **Partial index `WHERE` support** (citation: `src/index/create.rs:377-390`). The `ConditionalStatement` trait on `IndexCreateStatement` is a clean design — the partial predicate is a full `Expr` AST, not a raw string. Djogi should represent partial index predicates as structured expressions (not strings), whatever backend it uses.

- **Expression (functional) indexes** (citation: `src/index/common.rs:161-169`). Support for `Expr` as an index column is clean. Djogi will need this for lower() and similar.

- **`NULLS NOT DISTINCT`** (citation: `src/index/create.rs:303-306`). Useful for partial-null composite unique constraints — Djogi should support this in its index descriptor.

- **Table-level CHECK constraints via typed `Expr`** (citation: `src/table/create.rs:149-155`). The `Check { name, expr }` pattern with optional naming is the right shape for Djogi's check constraint descriptor.

- **`INCLUDE` columns** (citation: `src/index/create.rs:321-326`). Postgres covering indexes via `INCLUDE` are supported. Djogi may want this eventually.

- **Operator classes on index columns** (citation: `src/index/common.rs:51-62`). Useful for GIN/GiST indexes with specific operator classes. Djogi's JSONB indexing will need this.

- **`ColumnDef::extra(raw_string)` as an escape hatch** (citation: `src/table/column.rs:866-872`). Pragmatic safety valve for DDL features not yet covered. Djogi should have the same escape hatch on its column descriptor, even if Djogi formats SQL directly.

### Reject

- **sea-query as Djogi's DDL emitter** (decision: reject for now, possibly revisit). Djogi is Postgres-only and builds `NNNN_name_up.sql` files from a `build.rs` differ. sea-query's multi-dialect scaffolding (three backends, MySQL/SQLite specific features, type variants that don't map to Postgres) is dead weight for a Postgres-only tool. The `ColumnType` enum has `Year` which panics on Postgres (`src/backend/postgres/table.rs:114`); unsigned integer types silently map to signed Postgres types (`src/backend/postgres/table.rs:32-36`). Direct string formatting with careful Postgres-only `write!` calls is simpler, easier to audit, and produces no dependency. If Djogi later needs a multi-database target, sea-query becomes attractive.

- **sea-query's `Iden` derive macro in Djogi's own proc macro** (`Cargo.toml:32`). Djogi's `#[derive(Model)]` macro already generates its own identifier enum `{Model}Fields`. Adding sea-query as a dependency of `djogi-macros` imports the entire query-builder surface. Not worth it.

- **sea-query's `Value` enum for Djogi's parameters** — Djogi wraps SQLx directly and uses SQLx's own parameter binding. sea-query's `Value` type is a different runtime representation. Mixing them creates a translation layer with no benefit.

### Defer

- **Adopt sea-query if Djogi adds a second database target.** The `SchemaBuilder` abstraction is clean. If Djogi ever supports MySQL or SQLite, adopting sea-query for DDL emission would be a straightforward path.

- **Expression-based DEFAULT values via `Expr`** (citation: `src/table/column.rs:306-357`). sea-query's `ColumnDef::default(Expr)` supports arbitrary expressions including `Expr::current_timestamp()` and `Expr::val(x).add(y)`. Djogi currently uses string-level defaults; this pattern could inspire a typed-default descriptor. Defer until Djogi's descriptor format stabilizes.

### Surprises

- **`NULLS FIRST` / `NULLS LAST` not in index ordering.** `NullOrdering` exists in the SELECT ORDER BY query builder (`src/query/ordered.rs:3`) but was never extended to `IndexOrder` in `CREATE INDEX`. This is a gap — Postgres frequently uses `NULLS LAST` on B-tree indexes for nullable columns. sea-query cannot emit `("col" DESC NULLS LAST)` in a `CREATE INDEX`. Citation: `src/index/common.rs:67-70`.

- **`NOT VALID` / `VALIDATE CONSTRAINT` missing.** Two-phase constraint addition (add constraint `NOT VALID`, then `VALIDATE CONSTRAINT` in a separate transaction) is the canonical online-safe way to add FK or CHECK constraints to large tables. sea-query has no `TableAlterOption` for this. Citation: `src/table/alter.rs:56-64`. This is a significant online-safety gap.

- **No auto-naming.** sea-query never auto-generates constraint or index names. Every `ForeignKey::create()` and `Index::create()` is name-optional — but omitting the name produces `FOREIGN KEY (...)` without a `CONSTRAINT "name"` clause, making later `DROP CONSTRAINT` impossible by name. Djogi's differ must generate stable, deterministic names itself (sea-query will not help here).

- **`DEFERRABLE` constraints absent.** No path to emit `CONSTRAINT "name" FOREIGN KEY (...) DEFERRABLE INITIALLY DEFERRED` — the `TableForeignKey` struct has no deferrability field. Citation: `src/foreign_key/common.rs:5-13`.

- **`option-postgres-use-serial` is a compile-time feature, not a runtime choice.** You cannot mix `SERIAL` and `GENERATED ... AS IDENTITY` columns in the same binary. Citation: `src/backend/postgres/table.rs:233-246`. Not relevant for Djogi (which uses HeeRanjId), but illustrative of how dialect options are baked in.

- **`ColumnDef::extra(raw_string)` as universal escape hatch** (citation: `src/table/column.rs:866-872`, `src/table/create.rs:336-343`). When sea-query lacks a typed path (e.g., `DEFAULT gen_random_uuid()`, or `USING columnar` for Citus), `extra()` injects a raw string. This is pragmatic but unverified — the library does nothing to validate the injected SQL. Djogi's own string formatter has the same footgun.

- **The `TableAlterStatement` single-ALTER design** means that multiple subcommands on the same table go into one `ALTER TABLE ... op1, op2` statement. Postgres executes the whole batch atomically, which is correct — but it means you cannot interleave a `CONCURRENTLY` index creation (which must be outside a transaction) with other operations in the same alter batch. Djogi's build.rs differ must understand this separation already; sea-query would not help enforce it.
