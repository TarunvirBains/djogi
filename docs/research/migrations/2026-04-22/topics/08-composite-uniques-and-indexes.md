# Topic 08: Composite Uniques and Indexes

## Executive summary

Every surveyed system supports composite unique constraints and composite indexes in some form. The split is in representation depth: Django and SQLAlchemy have first-class ORM-level DSLs with optional naming conventions; Prisma has clean `@@unique` / `@@index` PSL annotations that compile to SQL; Liquibase requires explicit XML/YAML with a caller-supplied constraint name; Flyway and refinery are raw-SQL-only and provide zero representation. Diesel's `schema.rs` / `table!` macro does not model indexes or unique constraints at all — they are invisible to the framework and must live in hand-written SQL.

Partial indexes (`WHERE` clause) and functional/expression indexes (`ON t(LOWER(col))`) are the largest ecosystem gap. Only Django (via `Index(condition=Q(...))`) and SQLAlchemy (via `Index(..., postgresql_where=...)`) offer first-class typed support. Sea-query supports both as first-class flags. Prisma, Liquibase, Diesel, SeaORM (without sea-query escape), refinery, and cot all require raw SQL or have no support at all.

For Djogi: the naming convention and representation choice are open. The evidence strongly favors a single, explicit attribute-based approach on the model struct — no `unique_together` legacy dualism — with auto-generated names in the Postgres default pattern (`<table>_<col1>_<col2>_key` / `<table>_<col1>_<col2>_idx`) plus a user-override escape hatch. Column order must be preserved verbatim. Partial and functional indexes are an opportunity where Djogi can lead the Rust ecosystem.

---

## Comparison matrix

| System | Composite unique | Composite index | Naming | Partial (`WHERE`) | Functional (`EXPR`) | Ordering-preserving? |
|---|---|---|---|---|---|---|
| **Django** | `unique_together` (deprecated) or `UniqueConstraint(fields=[...])` | `Meta.indexes = [Index(fields=[...])]` | User-supplied `name=` required; auto-hash truncation for long names | Yes — `Index(condition=Q(...))` | Yes — `Index(F('col').lower())` | Yes |
| **SQLAlchemy** | `UniqueConstraint('a','b',name='...')` or naming convention | `Index('name',col_a,col_b)` | `naming_convention` tokens e.g. `uq_%(table_name)s_%(column_0_N_name)s` | Yes — `postgresql_where=` kwarg | Yes — pass SQL expression to `Index(expr)` | Yes |
| **Alembic** | `op.create_unique_constraint(name,table,[cols])` | `op.create_index(name,table,[cols])` | Caller-supplied; `naming_convention` on `MetaData` propagates | Via `postgresql_where=` from SQLAlchemy | Via expression column | Yes |
| **Prisma** | `@@unique([a, b])` in PSL | `@@index([a, b])` in PSL | Auto-generated or `name:` arg in PSL | No direct support; raw SQL only | No direct support | Yes |
| **Liquibase** | `<addUniqueConstraint columnNames="a,b" constraintName="..."/>` | `<createIndex indexName="..." tableName="..."><column name="a"/><column name="b"/></createIndex>` | `constraintName` / `indexName` required (no auto-gen) | No; raw SQL changeset only | No | Yes |
| **Flyway** | Raw SQL only | Raw SQL only | SQL-level only | SQL-level only | SQL-level only | Yes (SQL) |
| **Diesel** | Not in `schema.rs`; hand-written SQL only | Not in `schema.rs`; hand-written SQL only | SQL-level only | SQL-level only | SQL-level only | Yes (SQL) |
| **sea-query** | `Index::create().unique().col(a).col(b)` inline in `CREATE TABLE` or standalone | `Index::create().col(a).col(b)` | Caller-supplied; no auto-gen | Yes — `.and_where(expr)` via `ConditionalStatement` | Yes — `Expr` passed to `.col()` | Yes |
| **SeaORM** | Via sea-query `Index::create().unique()` in migration body | Via sea-query `Index::create()` | Caller-supplied string | No (sea-query escape hatch only) | No (sea-query escape hatch only) | Yes |
| **refinery** | Raw SQL only | Raw SQL only | SQL-level only | SQL-level only | SQL-level only | Yes (SQL) |
| **cot** | Not supported — `Field::unique()` is single-column only | Not supported — no `Operation` type for index creation | N/A | No | No | N/A |
| **Djogi (proposed)** | TBD | TBD | TBD | Opportunity | Opportunity | Must be yes |

---

## Composite unique constraints

### DB-level UNIQUE constraint vs UNIQUE index

Postgres implements a `UNIQUE` constraint by creating a B-tree index internally. However, there is a distinction at the catalog level:

- `ALTER TABLE t ADD CONSTRAINT name UNIQUE (a, b)` registers an entry in `pg_constraint` with `contype = 'u'` AND creates an underlying index. This constraint form is targetable by a foreign key (`REFERENCES t(a,b)`) and supports `ON CONFLICT ON CONSTRAINT name`.
- `CREATE UNIQUE INDEX name ON t (a, b)` creates an index registered only in `pg_indexes` (and `pg_index`). The index enforces uniqueness but is **not** in `pg_constraint` as a named unique constraint. `ON CONFLICT ON CONSTRAINT name` will not recognize it by default without `ALTER TABLE... ADD CONSTRAINT... USING INDEX name`.

Not all systems honour this distinction:

- **Django's `UniqueConstraint`** emits `ALTER TABLE... ADD CONSTRAINT... UNIQUE (...)` — it produces a proper `pg_constraint` entry. (Confidence: high — `operations/models.py:1143-1197`)
- **sea-query's inline `UNIQUE`** in `CREATE TABLE` produces `CONSTRAINT "name" UNIQUE (col1, col2)` — also a proper constraint. (Confidence: high — `tests/postgres/table.rs:327-335` verbatim)
- **sea-query's standalone `Index::create().unique()`** produces `CREATE UNIQUE INDEX` — index-only, not a constraint record. (Confidence: high — `src/index/create.rs:104-128`)
- **Django's legacy `unique_together`** emits `ALTER TABLE... ADD CONSTRAINT... UNIQUE (...)` in the same way as `UniqueConstraint`. (Confidence: high — `operations/models.py:702-711`)
- **Prisma** generates `CREATE UNIQUE INDEX` (not `ADD CONSTRAINT UNIQUE`). The test snapshot shows: `CREATE UNIQUE INDEX "Profile.userId" ON "Profile"("userId" ASC)`. (Confidence: high — `packages/migrate/src/__tests__/MigrateDiff.test.ts:574`)
- **SQLAlchemy's `UniqueConstraint`** renders `UNIQUE (a, b)` inline in `CREATE TABLE` or as `ALTER TABLE ADD CONSTRAINT`. It is a proper constraint, not just an index. (Confidence: high — `lib/sqlalchemy/sql/schema.py:5525-5534`, `lib/sqlalchemy/dialects/postgresql/base.py:1182-1193`)

**Implication for Djogi:** Djogi should generate `ALTER TABLE t ADD CONSTRAINT name UNIQUE (a, b)` — the constraint form, not the index form — for composite unique constraints declared via the model descriptor. This preserves `ON CONFLICT ON CONSTRAINT name` compatibility and FK targetability. Composite indexes (non-unique) should remain `CREATE INDEX`.

### Representation per system (verbatim)

#### Django

Legacy form (deprecated, pre-Django 2.2, still generated by older migration files):

```python
# operations/models.py:702-711
class AlterUniqueTogether(ModelOptionOperation):
  # Represents Meta.unique_together = [('field_a', 'field_b')]
  option_name = "unique_together"
```

Modern form (Django 2.2+, preferred):

```python
# operations/models.py:1143-1197
class AddConstraint(IndexOperation):
  # Represents Meta.constraints = [UniqueConstraint(fields=['field_a', 'field_b'], name='...')]
```

Model declaration syntax:

```python
class MyModel(models.Model):
  # Legacy — deprecated
  class Meta:
    unique_together = [('email', 'tenant')]

  # Modern
  class Meta:
    constraints = [
      UniqueConstraint(fields=['email', 'tenant'], name='mymodel_email_tenant_uniq')
    ]
```

Indexes:

```python
class Meta:
  indexes = [
    Index(fields=['last_name', 'first_name'], name='mymodel_last_first_idx'),
  ]
```

Django **requires** an explicit `name` on `Index` instances in migrations; `ModelState.__init__` enforces this with a `ValueError`. (Confidence: high — `django/db/migrations/state.py:777-783`)

Composite indexes in Django support partial predicates via the `condition=` parameter:

```python
# Partial index — Django's Meta.indexes with condition
Index(fields=['status', 'created_at'],
   name='active_orders_idx',
   condition=Q(status='active'))
```

Functional indexes use `F`-expression transforms:

```python
# Functional/expression index
Index(
  Lower('email'),
  name='mymodel_lower_email_idx'
)
```

(Confidence: high on the existence of these Django features from general knowledge; not directly traced to source lines in django.md because the research note does not enumerate all `Index` options beyond the required `name` enforcement. Label: **medium** — Django docs feature confirmed but no line citation from the project note.)

#### SQLAlchemy

```python
# lib/sqlalchemy/sql/schema.py:5525-5534
class UniqueConstraint(ColumnCollectionConstraint):
  __visit_name__ = "unique_constraint"
  # Composite: UniqueConstraint('a', 'b', name='uq_t_a_b')

# lib/sqlalchemy/sql/schema.py:5537-5685
class Index(DialectKWArgs, ColumnCollectionMixin, HasConditionalDDL, SchemaItem):
  def __init__(
    self,
    name,
    *expressions,   # Column objects, SQL expressions, text()
    unique=False,
    **dialect_kw,   # postgresql_where=, postgresql_using=, postgresql_ops=
  )
```

Composite unique constraint example:

```python
UniqueConstraint('email', 'tenant_id', name='uq_user_email_tenant')
```

Composite index example:

```python
Index('ix_user_last_first', user_table.c.last_name, user_table.c.first_name)
```

Partial index (Postgres):

```python
Index('ix_active_email', user_table.c.email,
   postgresql_where=(user_table.c.active == True))
```

Functional/expression index (Postgres):

```python
from sqlalchemy import func
Index('ix_lower_email', func.lower(user_table.c.email))
```

SQLAlchemy naming convention system — recommended fuller convention (not built-in default; must be passed to `MetaData`):

```python
# lib/sqlalchemy/sql/schema.py:5878-5926 (documented tokens)
convention = {
  "ix": "ix_%(column_0_label)s",
  "uq": "uq_%(table_name)s_%(column_0_N_name)s",
  "ck": "ck_%(table_name)s_%(constraint_name)s",
  "fk": "fk_%(table_name)s_%(column_0_N_name)s_%(referred_table_name)s",
  "pk": "pk_%(table_name)s",
}
MetaData(naming_convention=convention)
```

The `%(column_0_N_name)s` token joins **all** column names with underscores (e.g., `email_tenant_id`). (Confidence: high — `lib/sqlalchemy/sql/naming.py:130-136`, `lib/sqlalchemy/sql/schema.py:5878-5926`)

**Critical note from alembic.md:** Without a naming convention, anonymous constraints generate different names per run, causing spurious Alembic diffs. The `conv()` wrapper in `_InspectorConv` marks reflected constraint names so Alembic knows they were assigned by the naming convention, not hand-coded. (Confidence: high — `alembic/autogenerate/compare/util.py:87-102`)

#### Alembic (autogenerate output)

Composite unique constraint render (from `render.py`):

```python
# alembic/autogenerate/render.py:683, 691
# Composite unique constraints render as:
op.create_unique_constraint(name, table, [col_names_list])
# or inline in CreateTableOp:
sa.UniqueConstraint(*col_names)
```

The column list is rendered as `repr([_ident(col.name) for col in constraint.columns])`. (Confidence: high — `render.py:683`)

#### Prisma

PSL syntax for composite unique:

```prisma
model User {
 email  String
 tenantId Int
 @@unique([email, tenantId])
 // or with explicit name:
 @@unique([email, tenantId], name: "user_email_tenant_unique")
}
```

PSL syntax for composite index:

```prisma
model User {
 lastName String
 firstName String
 @@index([lastName, firstName])
 // or with explicit name:
 @@index([lastName, firstName], name: "user_last_first_idx")
}
```

Prisma compiles `@@unique([a, b])` to `CREATE UNIQUE INDEX` (not `ADD CONSTRAINT UNIQUE`). Test snapshot: `CREATE UNIQUE INDEX "Profile.userId" ON "Profile"("userId" ASC)`. (Confidence: high — `packages/migrate/src/__tests__/MigrateDiff.test.ts:574`)

If no `name:` is supplied in PSL, Prisma auto-generates a name. The exact naming algorithm lives in the Rust engine (`prisma-engines-reference`) and is not fully exposed in the TypeScript clone. Auto-names appear to use a dot-separated form like `"Profile.userId"` (from the test snapshot), which is not Postgres-idiomatic. (Confidence: medium — inferred from test snapshot only)

Prisma has **no direct support** for partial indexes or functional indexes. These require the user to drop down to raw SQL via `prisma db execute` or a custom migration SQL file. (Confidence: high — verified absence in source)

#### Liquibase

XML changeset form:

```xml
<!-- Composite unique constraint -->
<addUniqueConstraint
  tableName="users"
  columnNames="email, tenant_id"
  constraintName="uq_users_email_tenant"/>

<!-- Composite index -->
<createIndex indexName="idx_users_last_first" tableName="users">
  <column name="last_name"/>
  <column name="first_name"/>
</createIndex>
```

Liquibase requires an explicit `constraintName` / `indexName`. There is no auto-name generation. The generator emits either `ALTER TABLE t ADD UNIQUE (col1,col2)` or `ALTER TABLE t ADD CONSTRAINT name UNIQUE (col1,col2)` depending on whether a constraint name is provided. (Confidence: high — `AddUniqueConstraintGenerator.java:46-58`, `liquibase.md:247`)

Liquibase has **no built-in support** for partial or functional indexes. Users must use a `<sql>` or `<sqlFile>` changeset. (Confidence: high — verified absence in source)

#### Flyway

Flyway is raw-SQL-only. All constraint and index definitions are expressed as plain SQL in `V*.sql` migration scripts. There is no representation layer, no naming convention enforcement, and no typed API. (Confidence: high — `flyway.md` architecture section)

#### Diesel

Diesel's `schema.rs` and `table!` macro do **not** model secondary indexes or unique constraints. The `print_schema` pipeline queries `information_schema` only for PKs and FKs. Unique constraints are invisible to the framework; they must be created and dropped in hand-written `up.sql` / `down.sql`. (Confidence: high — `diesel.md:214-219`, `S5` surprise)

`--diff-schema` explicitly returns `Err(UnsupportedFeature("Tables with composite foreign keys are not supported by --diff-schema"))` for composite FK scenarios (diesel.md:S4), illustrating the general unimplemented state of multi-column DDL in the diff engine. Unique constraints and composite indexes are similarly outside scope. (Confidence: high)

#### sea-query

Two mechanisms for composite unique constraints (verbatim from source):

**Mechanism 1 — standalone `CREATE UNIQUE INDEX`:**

```rust
// src/index/create.rs:104-128 (doc-test, verbatim)
let index = Index::create()
 .name("idx-glyph-aspect")
 .table(Glyph::Table)
 .col((Glyph::Image, IndexOrder::Asc))
 .col((Glyph::Aspect, IndexOrder::Desc))
 .unique()
 .to_owned();
// Output: CREATE UNIQUE INDEX "idx-glyph-aspect" ON "glyph" ("image" ASC, "aspect" DESC)
```

**Mechanism 2 — inline in `CREATE TABLE` as `CONSTRAINT... UNIQUE`:**

```rust
// tests/postgres/table.rs:310-335 (verbatim test assert)
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
// Output: CREATE TABLE "glyph" ( "image" json, "aspect" jsonb, CONSTRAINT "idx-glyph-aspect-image" UNIQUE NULLS NOT DISTINCT ("aspect", "image") )
```

The `.index()` path inside `CREATE TABLE` produces the constraint form (`pg_constraint` entry); the standalone path produces a `CREATE UNIQUE INDEX`. (Confidence: high — `src/backend/postgres/index.rs:6-31`, `tests/postgres/table.rs:327-335`)

Partial index with `WHERE`:

```rust
// src/index/create.rs:155-172 (doc-test, verbatim)
let index = Index::create()
 .name("idx-glyph-aspect")
 .table(Glyph::Table)
 .col((Glyph::Aspect, 64, IndexOrder::Asc))
 .and_where(Expr::col((Glyph::Table, Glyph::Aspect)).is_in(vec![3, 4]))
 .to_owned();
// Output: CREATE INDEX "idx-glyph-aspect" ON "glyph" ("aspect" (64) ASC) WHERE "glyph"."aspect" IN (3, 4)
```

Functional/expression index — pass `Expr` to `.col()`:

```rust
// src/index/common.rs:161-169
// Expr objects can be passed directly as index columns
// e.g. Expr::col(col).lower() for LOWER(col)
```

(Confidence: high — `src/index/common.rs:161-178`, `tests/postgres/index.rs:132-141`)

Naming: sea-query **requires** the caller to supply an explicit name string. There is no auto-generation of names from table/column combinations. Omitting `.name()` produces an index without a name, making later `DROP INDEX` impossible by name. (Confidence: high — `src/index/create.rs:258-263`, sea-query.md surprise note)

#### SeaORM

SeaORM delegates all composite unique and index DDL to sea-query, used inside migration `up()` methods:

```rust
// sea-orm-reference (sea-orm.md:221-237 pattern)
manager.create_index(
  Index::create()
   .unique()
   .name("idx_user_email_name")
   .table(User::Table)
   .col(User::Email)
   .col(User::Name)
   .to_owned()
).await?;
```

At the entity level, `#[sea_orm(unique)]` is **single-column only**. Composite uniques require an explicit `Index::create()` call in the migration body. (Confidence: high — `sea-orm.md:219-237`)

#### refinery

refinery has no awareness of composite unique constraints, composite indexes, or any schema constructs. It is a pure migration runner; SQL is expressed in raw `.sql` files. (Confidence: high — `refinery.md:247-254`)

#### cot

cot does not support composite unique constraints as a migration operation. `Field::unique()` marks a single column `UNIQUE`. There is no `UniqueConstraint` builder anywhere in `Operation` or `OperationInner`. There is no index-creation operation at all. (Confidence: high — `cot.md:271-276`)

#### Djogi (proposed)

See section "Djogi implications" below.

---

## Composite indexes

### Representation per system

The pattern for composite indexes closely mirrors composite uniques for each system. The key differences:

- **Composite indexes** do not create a `pg_constraint` entry; they are always `CREATE INDEX` in the catalog.
- **Unique composite indexes** via `CREATE UNIQUE INDEX` are also not constraint-form; they enforce uniqueness but are only in `pg_index`.

Systems that represent composite indexes:

- **Django:** `Meta.indexes = [Index(fields=['a', 'b'], name='...')]` — AddIndex migration operation. Name is mandatory. Autodetector generates `AddIndex` / `RemoveIndex` / `RenameIndex` operations. Rename detection: if everything matches except the name, a `RenameIndex` is emitted instead of remove + add. (Confidence: high — `django.md:377-391`)

- **SQLAlchemy:** `Index('name', col_a, col_b)` — a first-class schema object, part of `Table.indexes`. Explicitly separate from `Table.constraints`. (Confidence: high — `sqlalchemy.md:267-288`)

- **Alembic:** `op.create_index(name, table, [cols])` in migration scripts. Autogenerate detects added/removed/changed indexes via `_compare_indexes_and_uniques()` in `compare/constraints.py`. (Confidence: high — `alembic.md:321`)

- **Prisma:** `@@index([a, b])` in PSL — compiles to `CREATE INDEX`. Engine handles the SQL generation. (Confidence: high — `prisma.md:254`)

- **Liquibase:** `<createIndex indexName="..." tableName="..."><column name="a"/><column name="b"/></createIndex>`. Column ordering is preserved by the order of `<column>` children. (Confidence: high — `liquibase.md:248`)

- **sea-query:** `Index::create().name("...").table(T).col(A).col(B)` — chained `.col()` calls. Per-column sort order via `IndexOrder::Asc` / `IndexOrder::Desc` tuple. (Confidence: high — `sea-query.md:248-252`)

- **SeaORM:** via sea-query in migration body, same as above. (Confidence: high — `sea-orm.md:237-239`)

Systems that do NOT represent composite indexes at the framework level:

- **Diesel** — schema.rs has no index concept. All indexes are hand-written SQL.
- **Flyway** — raw SQL only.
- **refinery** — raw SQL only.
- **cot** — no index operation type at all.

---

## Naming conventions

### Postgres default: `<table>_<col>_<col>_key` and `<table>_<col>_<col>_idx`

Postgres, when creating a constraint with `ADD CONSTRAINT... UNIQUE (a, b)`, **does not auto-name it** — the user must supply a name in `ADD CONSTRAINT name UNIQUE`. If no name is given (`ADD UNIQUE (a, b)` without `CONSTRAINT name`), Postgres auto-generates a name in the form `<table>_<col1>_<col2>_key`. For example:

```sql
ALTER TABLE users ADD UNIQUE (email, tenant_id);
-- Postgres auto-generates: users_email_tenant_id_key
```

For plain indexes, the Postgres default auto-name (when `CREATE INDEX` is used without a name) is `<table>_<col1>_<col2>_idx`. However, in practice all ORMs and migration tools surveyed require or encourage explicit naming for tracking purposes.

### Django's 15-character truncation and hash

Django enforces that index names are provided by the user and raises an error if missing. However, when index names exceed Oracle's 30-character limit for older Alembic compatibility reasons, Django uses a hash-based truncation. The algorithm (from general Django knowledge; not directly cited in django.md because the note does not capture the truncation code) produces a 15-character suffix by hashing the full name and appending it to a truncated prefix.

The note at `django.md` confirms: composite unique constraints over FK fields work correctly because the autodetector emits `AlterUniqueTogether` after the FK `AddField` operations. (Confidence: high — `autodetector.py:836-844`)

### SQLAlchemy naming convention tokens

The `%(column_0_N_name)s` token is the key pattern for composite constraint names. It joins all column names in the constraint with underscores. With `MetaData(naming_convention={"uq": "uq_%(table_name)s_%(column_0_N_name)s"})`:

- `UniqueConstraint('email', 'tenant_id')` on table `users` → name = `uq_users_email_tenant_id`

The token `column_0N_name` (no separator between `0` and `N`) joins without separator. The `column_0_N_name` (with underscores) joins with underscores. The choice matters for readability. (Confidence: high — `sqlalchemy.md:330-355`, `lib/sqlalchemy/sql/naming.py:103-127`)

Without a naming convention, unnamed constraints generate different names on each database reset, causing false Alembic diffs. This is the primary motivation for the convention system. (Confidence: high — `alembic.md:393-399`)

### Prisma naming

Prisma auto-generates constraint/index names if `name:` is not provided in PSL. The auto-name pattern from the test snapshot is `"Profile.userId"` (dot-separated table and column names), which is not Postgres-idiomatic. For multi-column cases the pattern is inferred but not explicitly documented in the surveyed source. Users can always supply an explicit `name:` argument. (Confidence: medium — inferred from test snapshot at `packages/migrate/src/__tests__/MigrateDiff.test.ts:574`)

### Liquibase: explicit `constraintName=` required

Liquibase requires an explicit `constraintName` for all `addUniqueConstraint` operations and an explicit `indexName` for all `createIndex` operations. There is no auto-generation of names. If the user omits the constraint name in the XML, the database's own auto-naming fires (e.g., Postgres's `_key` suffix). (Confidence: high — `liquibase.md:247-249`)

### Flyway, refinery, Diesel: SQL-level naming only

These systems have no naming layer. Names are whatever appears in the SQL file. The database's default naming applies if no `CONSTRAINT name` clause is present.

### sea-query: mandatory caller-supplied name

sea-query's `Index::create()` and `ForeignKey::create()` require the caller to supply a name string. Omitting the `.name()` call produces a statement without a `CONSTRAINT` clause, making later `DROP INDEX`/`DROP CONSTRAINT` impossible by name. (Confidence: high — `sea-query.md:245`, `src/index/create.rs:258-263`)

### cot: N/A (no composite index support)

---

## Ordering preservation

Postgres's B-tree index is ordered by the column sequence declared in the index definition:

- `UNIQUE (a, b)` supports `WHERE a = ?` (leading prefix), `WHERE a = ? AND b = ?` (full match), but does NOT efficiently support `WHERE b = ?` (middle column scan without leading column).
- `UNIQUE (b, a)` is the transposed index — it supports `WHERE b = ?` but not `WHERE a = ?` without `b`.

These two are **not equivalent** for query performance. They may appear equivalent for uniqueness enforcement, but the index column order determines which query patterns use the index via an index scan vs. a full table scan.

All systems surveyed that represent composite indexes preserve column order:

- **Django:** `fields=['a', 'b']` — the list order is the index column order. The autodetector stores the field list and emits it in order. (Confidence: high — `django.md:379-391`)
- **SQLAlchemy:** `Index('name', col_a, col_b)` — positional arguments define the column order. (Confidence: high — `sqlalchemy.md:267-288`)
- **Prisma:** `@@index([a, b])` — list order is preserved. (Confidence: high — `prisma.md:254`)
- **sea-query:** `.col(A).col(B)` — chained in the order of `.col()` calls; each call appends to `TableIndex.columns`. (Confidence: high — `src/index/common.rs:196-199`)
- **Liquibase:** `<column>` children in document order. (Confidence: high — `liquibase.md:248`)

**No system surveyed supports specifying index column order independently from the constraint column order.** If a `UNIQUE (a, b)` constraint is needed for uniqueness but the index should be ordered `(b, a)` for a particular query pattern, the user must create both (or choose one and accept the tradeoff).

**Implication for Djogi:** The descriptor representation of composite unique constraints and indexes must preserve column order exactly as written by the user. The differ must treat a change in column order as a full drop-and-recreate, not a rename. `UNIQUE (a, b)` and `UNIQUE (b, a)` are different descriptors that produce different indexes.

---

## Partial indexes (`WHERE` clause)

Partial indexes index only the rows satisfying a predicate. Example:

```sql
CREATE INDEX idx_active_users ON users (email) WHERE status = 'active';
```

This index is smaller than a full index and supports faster queries filtered by `status = 'active'`.

### Support across systems

**Django:** First-class support via `Index(condition=Q(...),...)`. The `condition` parameter accepts a Django `Q` object (ORM filter expression). Compiled to `WHERE` clause in the generated SQL. Supported in `AddIndex` migrations. (Confidence: medium — Django docs feature; not directly cited in django.md research note source lines, but the note captures the Index operation's existence and Django's `Q` expression system)

**SQLAlchemy:** First-class via `postgresql_where=` dialect keyword argument:

```python
Index('ix_active_users', user_table.c.email,
   postgresql_where=(user_table.c.status == 'active'))
```

(Confidence: high — `sqlalchemy.md` via `lib/sqlalchemy/sql/schema.py:5537-5685`, `**dialect_kw` accepting `postgresql_where`)

**Alembic:** Inherits from SQLAlchemy. Autogenerate detects index differences; partial index predicates are included in the comparison if represented in metadata. (Confidence: high — Alembic uses SQLAlchemy `Index` objects)

**sea-query:** First-class via `ConditionalStatement` trait on `IndexCreateStatement`. `.and_where(expr)` adds a `WHERE` clause. Verbatim output confirmed:

```rust
// src/index/create.rs:155-172 (doc-test)
// Output: CREATE INDEX "idx-glyph-aspect" ON "glyph" ("aspect" (64) ASC) WHERE "glyph"."aspect" IN (3, 4)
```

(Confidence: high — `sea-query.md:110-112`, `src/index/create.rs:377-390`)

**Prisma:** No direct support. Users must write raw SQL in a custom migration file or via `prisma db execute`. (Confidence: high — verified absence in source)

**Liquibase:** No built-in support in the typed change DSL. Users must use `<sql>` changesets with raw SQL. (Confidence: high — verified absence in source)

**Flyway, Diesel, refinery, cot:** Raw SQL only or no index support at all. (Confidence: high)

**SeaORM:** Accessible only via the underlying sea-query builder inside a migration body (use `manager.execute(Statement::from_string(...))` or sea-query's `.and_where()`). No direct `SchemaManager` API for partial indexes. (Confidence: high — `sea-orm.md`, no `has_partial_index` or WHERE API on `SchemaManager`)

### Ecosystem gap

**Partial index support is underserved in the Rust ORM/migration ecosystem.** Diesel, refinery, cot, and SeaORM (at the migration manager API level) all require raw SQL. Sea-query provides the lowest-level typed support in Rust, but SeaORM does not surface it cleanly.

---

## Functional / expression indexes

Functional indexes use an expression instead of a bare column:

```sql
CREATE INDEX idx_lower_email ON users (LOWER(email));
```

This supports case-insensitive uniqueness enforcement and queries like `WHERE LOWER(email) = LOWER('foo@example.com')`.

### Support across systems

**Django:** Supported via `F`-expression transforms:

```python
Index(Lower('email'), name='idx_lower_email')
```

The `Lower()` is a database function object. (Confidence: medium — Django docs feature; not directly cited in the django.md research note source lines)

**SQLAlchemy:** Full expression support:

```python
from sqlalchemy import func
Index('ix_lower_email', func.lower(user_table.c.email))
```

The `expressions` parameter to `Index.__init__` accepts arbitrary SQL expressions. (Confidence: high — `sqlalchemy.md:267-288`, `lib/sqlalchemy/sql/schema.py:5537-5685`)

**sea-query:** First-class via `Expr` passed to `.col()`:

```rust
// src/index/common.rs:161-169
// Expr can be passed as an index column
// e.g. Expr::col(col).lower()
```

Test confirmed at `tests/postgres/index.rs:132-141`. (Confidence: high — `sea-query.md:111`)

**Prisma:** No direct support. Raw SQL migration required. (Confidence: high — verified absence)

**Liquibase:** No typed support. Raw `<sql>` changeset. (Confidence: high)

**Flyway, Diesel, refinery, cot:** Raw SQL or no support. (Confidence: high)

### Ecosystem gap

Functional indexes are similarly underserved in the Rust ecosystem. Sea-query provides typed support at the builder level; no higher-level Rust ORM/migration tool exposes this to the user without requiring them to write raw SQL.

---

## Reflection / introspection

Can a system read an existing database and determine which indexes were created by which migration?

| System | Can reflect composite indexes? | Can determine migration origin? |
|---|---|---|
| **Alembic** | Yes — `_compare_indexes_and_uniques()` reflects `get_indexes()` and `get_unique_constraints()` via Inspector | Yes — autogenerate compares reflected state against metadata |
| **Django** | Yes — `inspectdb` reads live DB and emits model code (one-shot, not migration system) | No — no migration-to-index mapping |
| **Liquibase** | Yes — `generateChangeLog` / `snapshot` command reads live DB into `DatabaseSnapshot` | Yes — `DiffToChangeLog` produces changesets, implicitly linking each to a migration |
| **Prisma** | Yes — `db pull` / `introspect` RPC reads live DB schema | No — the engine produces PSL, not a per-migration attribution |
| **SQLAlchemy** | Yes — `Inspector.get_indexes()`, `Inspector.get_unique_constraints()` | No — schema metadata layer only; Alembic provides migration context |
| **Flyway** | No — Flyway is pure SQL execution; no schema reflection | No |
| **Diesel** | Partial — `print-schema` emits typed column declarations but not indexes | No |
| **sea-query** | No — emit-only, no parser or introspection | N/A |
| **SeaORM** | Partial — `has_table`, `has_column`, `has_index` via sea-schema; full schema only via `generate entity` | No |
| **refinery** | No | No |
| **cot** | No — sea-schema not used; snapshot-based only | No |

**Key finding:** Alembic's autogenerate is the gold standard for reflection-driven diff. It reflects `get_indexes()` and `get_unique_constraints()` from the database catalog, converts them to `_constraint_sig` objects for consistent comparison, then computes added/removed/changed indexes by name and column-set signature. The naming convention system is critical here — without deterministic names, comparison fails. (Confidence: high — `alembic.md:321-323`, `alembic/autogenerate/compare/constraints.py:53-441`)

Liquibase's `generateChangeLog` can produce a full changelog from a live database, attributing each object to a changeset. This is the strongest migration-origin attribution in the surveyed set. (Confidence: high — `liquibase.md:248-251`)

**Implication for Djogi:** Djogi's differ operates against the `schema_snapshot.json` (descriptor-driven), not live introspection. However, for the `djogi baseline` workflow (adopting an existing database), a reflection step similar to Alembic's `Inspector` or Liquibase's `generateChangeLog` will be needed to detect existing indexes and produce the initial snapshot. Composite unique constraints must be detectable by name during this step — which requires Djogi to generate deterministic, stable names from day one.

---

## Convergence and divergence

### Universal convergence

- All systems that have a DSL (Django, SQLAlchemy/Alembic, Prisma, sea-query, SeaORM, Liquibase) support basic composite unique constraints and composite indexes.
- All systems preserve column order in composite definitions.
- All systems that have auto-naming or naming conventions use some form of `<table>_<col(s)>` pattern.

### Significant splits

- **Constraint form vs. index form:** Django `UniqueConstraint`, sea-query inline, SQLAlchemy `UniqueConstraint` → constraint record in `pg_constraint`. Prisma `@@unique`, sea-query standalone → `CREATE UNIQUE INDEX` only. Liquibase `addUniqueConstraint` with name → constraint form. This distinction matters for `ON CONFLICT ON CONSTRAINT name` and FK targetability.

- **Partial indexes:** Django and SQLAlchemy/Alembic (Python side), sea-query (Rust side) have first-class support. Everything else is raw SQL or absent.

- **Functional/expression indexes:** Same split — Django, SQLAlchemy, sea-query have typed support; the rest require raw SQL.

- **Auto-naming:** SQLAlchemy with naming convention is the most sophisticated system. Django requires user-supplied names (enforced). Prisma auto-generates but the algorithm is opaque. Liquibase requires explicit names. sea-query requires explicit names. Raw-SQL-only systems: no naming layer.

- **Reflection-driven diff:** Alembic and Liquibase can detect existing composite indexes in a live DB and generate corresponding migration operations. Django `inspectdb` is a one-shot code generation tool. Prisma `db pull` generates PSL. Others cannot reflect index structures into their migration system.

---

## Djogi implications

### Recommendation 1: Single first-class composite unique descriptor

Do **not** build two representations like Django's `unique_together` + `UniqueConstraint`. Django's lessons note explicitly: `unique_together` as a legacy path is a mistake Django maintains for backwards compat, and Djogi should use a single, first-class `UniqueConstraint` model from day one. (Confidence: high — `django.md:531`)

Proposed Djogi syntax (on the model struct):

```rust
#[derive(Model)]
#[djogi::unique(fields = [email, tenant_id], name = "uq_users_email_tenant")]
#[djogi::unique(fields = [phone, tenant_id])] // name auto-generated if omitted
pub struct User {
  pub email: String,
  pub tenant_id: i64,
  pub phone: Option<String>,
}
```

Or as a `meta` block:

```rust
#[derive(Model)]
#[djogi(
  unique = [(email, tenant_id, name = "uq_users_email_tenant")],
  indexes = [(last_name, first_name, name = "idx_users_last_first")]
)]
pub struct User {... }
```

The exact Rust attribute syntax is unresolved, but the semantic requirements are:
- Accept one or more field name lists
- Accept an optional explicit name; auto-generate a default name if omitted
- Preserve column order
- Support `where = "..."` for partial indexes
- Support `expr = "..."` or typed expression for functional indexes

### Recommendation 2: SQL generation — constraint form for UNIQUE

Djogi's differ should generate `ALTER TABLE t ADD CONSTRAINT name UNIQUE (a, b)` for composite unique constraints declared via the descriptor, not `CREATE UNIQUE INDEX`. Rationale:
- `pg_constraint` registration enables `ON CONFLICT ON CONSTRAINT name`
- FK targetability (a FK can reference a `UNIQUE` constraint but not a bare unique index)
- Django, SQLAlchemy, and sea-query's inline form all produce constraint form

For non-unique composite indexes, generate `CREATE INDEX name ON t (a, b)`.

### Recommendation 3: Default naming convention

When a composite unique or index name is not user-supplied, auto-generate using the Postgres-native pattern:

- Unique constraint: `<table>_<col1>_<col2>_key` (matches Postgres default for `ADD UNIQUE`)
- Index: `<table>_<col1>_<col2>_idx`

For long names (Postgres identifiers are limited to 63 bytes), truncate deterministically. Django's approach is to hash the full name and take 15 chars as a suffix. Djogi should define a similar algorithm:
1. Compute the full name using the `<table>_<col1>_<col2>_key/idx` pattern.
2. If `len(full_name) <= 63`: use as-is.
3. If `len(full_name) > 63`: take the first 48 bytes of the full name + `_` + first 8 hex chars of SHA-256 of the full name. This gives a stable, collision-resistant 57-byte name.

This mirrors the spirit of Django's hash-truncation without requiring a regex (which Djogi explicitly forbids in `CLAUDE.md`).

### Recommendation 4: Preserve column order as a first-class invariant

The differ must treat `UNIQUE (a, b)` and `UNIQUE (b, a)` as **distinct** and incompatible. A column-order change must trigger a `DROP CONSTRAINT` + `ADD CONSTRAINT` (or index: `DROP INDEX` + `CREATE INDEX`). The descriptor diff must compare column lists as ordered sequences, not sets.

### Recommendation 5: Partial index support

Partial indexes are a genuine gap in the Rust ecosystem. Sea-query supports them at the builder level, but no higher-level Rust ORM (cot, SeaORM, Diesel) exposes them cleanly. Djogi has the opportunity to lead.

Proposed syntax:

```rust
#[djogi(
  indexes = [
    (email, where = "deleted_at IS NULL", name = "idx_users_active_email")
  ]
)]
pub struct User {... }
```

SQL generated:

```sql
CREATE INDEX idx_users_active_email ON users (email) WHERE deleted_at IS NULL;
```

The `where` value is a raw SQL predicate string. This is pragmatic: a typed expression AST for WHERE predicates is substantial build-out; a raw string is safe at the database level and avoids needing an embedded SQL parser (which Djogi's no-regex policy would also constrain). The string should be included verbatim in the generated migration SQL file and checksummed as part of the migration file content.

### Recommendation 6: Functional / expression index support

Similarly:

```rust
#[djogi(
  indexes = [
    (expr = "lower(email)", name = "idx_users_lower_email")
  ]
)]
pub struct User {... }
```

SQL generated:

```sql
CREATE INDEX idx_users_lower_email ON users (lower(email));
```

Again, the expression is a raw SQL string included verbatim. This matches how SQLAlchemy handles it for simple cases (`func.lower(col)`) and how sea-query handles it (pass `Expr` to `.col()`).

### Recommendation 7: Distinguish composite unique from composite index in the descriptor

Do not conflate `unique` and `index` — they produce different SQL and have different catalog semantics:

- `unique = [(a, b)]` → `ALTER TABLE t ADD CONSTRAINT name UNIQUE (a, b)` (constraint form)
- `indexes = [(a, b)]` → `CREATE INDEX name ON t (a, b)` (plain index)
- `indexes = [(a, b, unique = true)]` → This case should probably be rejected in favor of `unique = [...]` to keep the distinction clean.

### Recommendation 8: Reflection for baseline adoption

For `djogi baseline` against an existing database, Djogi will need to detect existing composite unique constraints and indexes. This requires querying `pg_constraint` for unique constraints and `pg_indexes` / `pg_index` for plain indexes. The naming convention must be stable enough that re-generating descriptors from a reflected schema produces the same names — otherwise baseline adoption will trigger false diffs on the next migration run.

---

## Open questions

1. **Djogi attribute syntax for composite constraints:** Should it be a proc-macro attribute on the struct, a field inside a `#[djogi(...)]` meta block, or a separate `#[djogi::constraint(...)]` annotation? The choice affects how `djogi-macros` parses and emits the descriptor.

2. **Name collision handling:** If two composite unique constraints or indexes on the same table auto-generate to the same name (e.g., due to truncation), Djogi should detect this at codegen time and emit a compile error, not silently truncate both to the same string and produce a migration that fails at runtime.

3. **`NULLS NOT DISTINCT` support:** Sea-query supports `.nulls_not_distinct()` on `IndexCreateStatement`. This is a Postgres 15+ feature that treats all `NULL` values as equal for uniqueness purposes. Djogi should expose this for composite unique constraints on nullable columns. No other surveyed system (Django, SQLAlchemy, Liquibase, Prisma) was confirmed to support this in a typed way.

4. **Concurrent index creation:** `CREATE INDEX CONCURRENTLY` cannot run inside a transaction. If Djogi's descriptor declares an index, the differ must emit the `CREATE INDEX` in a non-transactional migration segment. Whether this means auto-splitting the migration file or requiring the user to annotate the index as `concurrent = true` is unresolved.

5. **Operator classes (`text_pattern_ops`, `jsonb_ops`, etc.):** Sea-query supports `.with_operator_class(iden)` on `IndexColumn`. Djogi will need this for GIN/GiST indexes on JSONB columns (core to Djogi's `Jsonb<T>` feature). This is out of scope for basic composite B-tree indexes but should be in the descriptor design from the start.

6. **`INCLUDE` covering indexes:** Sea-query supports `.include(col)` on `IndexCreateStatement` for Postgres covering indexes. Whether Djogi's descriptor should expose this is open.

7. **Diff identity for renamed indexes:** Django's autodetector detects renamed indexes (same column set, different name) and emits `RenameIndex` instead of `RemoveIndex` + `AddIndex`. Djogi's differ should do the same for composite indexes, since `DROP INDEX` + `CREATE INDEX` requires an exclusive lock while `ALTER INDEX... RENAME TO` does not.
