# SQLAlchemy (schema metadata only)

## Metadata
- Clone path: `/home/tarunvir/projects/sqlalchemy-reference/`
- Commit SHA inspected: `deb949fe05ed8ff0f72f01d53f08f21ba8776aef` (origin/main, fetched 2026-04-22; the local repo had an empty working tree so files were checked out via `git checkout origin/main -- lib/`)
- Primary language: Python
- Scope of this note: schema metadata and DDL generation (explicitly NOT the ORM or query layer)
- Key modules inspected:
 - `lib/sqlalchemy/sql/schema.py` (6703 lines)
 - `lib/sqlalchemy/sql/naming.py` (210 lines)
 - `lib/sqlalchemy/sql/ddl.py` (1928 lines)
 - `lib/sqlalchemy/sql/compiler.py` (DDLCompiler section)
 - `lib/sqlalchemy/engine/reflection.py` (2100 lines)
 - `lib/sqlalchemy/dialects/postgresql/base.py` (5789 lines)
 - `lib/sqlalchemy/dialects/postgresql/pg_catalog.py`
 - `lib/sqlalchemy/dialects/postgresql/named_types.py`
 - `lib/sqlalchemy/dialects/postgresql/types.py`
 - `alembic-reference/alembic/autogenerate/compare/server_defaults.py`

---

## Architecture

### MetaData as registry

`MetaData` is the root container. It holds `tables` (a `FacadeDict` keyed by table name or `schema.tablename`) and optional `naming_convention`. Tables register themselves into it at construction time — passing the same name and `MetaData` twice returns the same object. (`lib/sqlalchemy/sql/schema.py:5790–5954`)

`Table` carries:
- `constraints: Set[Constraint]` — all constraint objects including PK, FK, unique, check
- `indexes: Set[Index]` — separate collection, because indexes are not constraints in SQL (despite Postgres implementing unique constraints via an index)
- `primary_key: PrimaryKeyConstraint` — always present; auto-created implicitly from `primary_key=True` columns
- `foreign_key_constraints` — subset of `constraints`

(`lib/sqlalchemy/sql/schema.py:421–445`)

### DDL compilation pipeline

DDL compilation uses the visitor pattern throughout. The key class is `DDLCompiler` (in `lib/sqlalchemy/sql/compiler.py:6990`), which is instantiated by the dialect:

```python
def _compiler(self, dialect, **kw):
  return dialect.ddl_compiler(dialect, self, **kw)
```
(`lib/sqlalchemy/sql/ddl.py:87`)

`DDLCompiler` contains `visit_create_table`, `visit_create_index`, `visit_check_constraint`, `visit_primary_key_constraint`, `visit_foreign_key_constraint`, `visit_unique_constraint`. The PostgreSQL dialect subclasses this as `PGDDLCompiler` (`lib/sqlalchemy/dialects/postgresql/base.py:2504`) and overrides many of these methods to add Postgres-specific DDL.

`SchemaGenerator` and `SchemaDropper` (in `lib/sqlalchemy/sql/ddl.py:1347` and `1523`) are visitor classes that walk the schema graph and emit CREATE/DROP calls. They are not the DDL string builders — they orchestrate the order of operations and delegate string production to `DDLCompiler`.

Type rendering is separated into `GenericTypeCompiler` (base) and `PGTypeCompiler` (Postgres override, `lib/sqlalchemy/dialects/postgresql/base.py:2929`).

### Reflection architecture

The `Inspector` class (`lib/sqlalchemy/engine/reflection.py:182`) is the public API. It is obtained via `inspect(engine)`. Internally it delegates to dialect-specific methods: `get_columns`, `get_pk_constraint`, `get_indexes`, `get_unique_constraints`, `get_foreign_keys`, `get_check_constraints`. For PostgreSQL, these are on `PGDialect` (base.py).

Rather than raw SQL strings, the PG dialect builds its queries using SQLAlchemy's own expression language against `Table` objects defined in `lib/sqlalchemy/dialects/postgresql/pg_catalog.py` — a model of the `pg_catalog` system tables. This is a critical design choice: the introspection queries are composable, parameterized, and type-safe.

---

## Schema object model

### `Table`

Confidence: **high** (directly read from source)

Key attributes:
- `name`, `schema` — table name and schema; combined as `key` (`schema.name` or just `name`)
- `columns` — ordered column collection
- `primary_key: PrimaryKeyConstraint` — always present
- `constraints: Set[Constraint]` — PK + FK + unique + check
- `indexes: Set[Index]` — separate from constraints
- `foreign_key_constraints` — subset of constraints
- `_prefixes` — allows `CREATE TEMPORARY TABLE`, etc.

(`lib/sqlalchemy/sql/schema.py:327–445`)

### `Column`

Confidence: **high**

Constructor signature (relevant attributes for migration systems):

```python
Column(
  name, type_,
  autoincrement="auto",   # "auto" | True | False | "ignore_fk"
  nullable=SchemaConst.NULL_UNSPECIFIED, # three-way: True/False/unspecified
  primary_key=False,
  unique=None,
  index=None,
  server_default=None,   # FetchedValue | str | TextClause | ColumnElement
  server_onupdate=None,
  comment=None,
  system=False,
  insert_sentinel=False,
)
```
(`lib/sqlalchemy/sql/schema.py:1779–1812`)

`nullable` defaults to `SchemaConst.NULL_UNSPECIFIED` (not `True` or `False`). Djogi note: three-way nullability at the metadata level, but PG DDL will always emit NOT NULL or omit it.

`autoincrement="auto"` means: single-column integer PK with no explicit default gets SERIAL/BIGSERIAL in Postgres DDL automatically. Explicit `Identity()` overrides this. (`lib/sqlalchemy/dialects/postgresql/base.py:2516–2537`)

`server_default` is typed as `Optional[FetchedValue]`, but the constructor accepts `str | TextClause | ColumnElement` and wraps them in `DefaultClause`. The distinction matters for autogenerate (see server defaults section). (`lib/sqlalchemy/sql/schema.py:2421–2428`)

### `MetaData.sorted_tables`

Confidence: **high**

```python
def sorted_tables(self) -> List[Table]:
  return ddl.sort_tables(
    sorted(self.tables.values(), key=lambda t: t.key)
  )
```
(`lib/sqlalchemy/sql/schema.py:6034–6079`)

Performs topological sort by FK dependency. Returns tables in creation order (reversed = drop order). When FK cycles are detected, the FK edges involved in cycles are excluded from the sort and a warning is emitted. `use_alter=True` on `ForeignKeyConstraint` is the escape hatch — it defers those FKs to `ALTER TABLE` after all tables are created.

**Djogi implication:** Djogi needs an equivalent topological sort for its migration plan ordering. The cycle-breaking mechanism (`use_alter`) is also needed for self-referential or mutually-referential FK relationships.

### `MetaData.naming_convention`

See dedicated section below. The default is:

```python
DEFAULT_NAMING_CONVENTION: _NamingSchemaParameter = util.immutabledict(
  {"ix": "ix_%(column_0_label)s"}
)
```
(`lib/sqlalchemy/sql/schema.py:5785–5787`)

Only indexes are auto-named by default. All other constraint types are unnamed unless the user provides a naming convention.

### Types

Confidence: **high** (class structure), **medium** (PG-specific rendering details)

Base class is `TypeEngine` in `lib/sqlalchemy/sql/type_api.py`. `TypeDecorator` wraps another type with Python-side pre/post-processing. The DDL string is produced by the `TypeCompiler` visitor: `GenericTypeCompiler` for generic SQL, `PGTypeCompiler` for Postgres.

`PGTypeCompiler` renders Postgres-specific types as simple keyword strings (`lib/sqlalchemy/dialects/postgresql/base.py:2929–3004`):
- `visit_INET` → `"INET"`
- `visit_CIDR` → `"CIDR"`
- `visit_TSVECTOR` → `"TSVECTOR"`
- `visit_JSONB` → `"JSONB"`
- `visit_JSON` → `"JSON"`
- `visit_HSTORE` → `"HSTORE"`
- `visit_CITEXT` → `"CITEXT"`

All Postgres-specific types live in `lib/sqlalchemy/dialects/postgresql/types.py` (INET, CIDR, MACADDR, MONEY, BIT) and companion files (`json.py` for JSON/JSONB, `hstore.py`, `array.py`, `ranges.py`, `named_types.py` for ENUM).

---

## Constraints and indexes

### `Constraint` base class

Confidence: **high**

All constraints inherit from `Constraint`:

```python
class Constraint(DialectKWArgs, HasConditionalDDL, SchemaItem):
  def __init__(
    self,
    name=None,
    deferrable=None,  # Optional[bool] → DEFERRABLE / NOT DEFERRABLE
    initially=None,  # Optional[str] → INITIALLY DEFERRED / IMMEDIATE
    info=None,
    comment=None,
   ...
  )
```
(`lib/sqlalchemy/sql/schema.py:4469–4541`)

`deferrable` and `initially` are first-class attributes on all constraints, not Postgres-specific. This is correct — they are standard SQL.

`ColumnCollectionConstraint` (the intermediate base for all column-collection constraints) is at `lib/sqlalchemy/sql/schema.py:4732`.

### `PrimaryKeyConstraint`

Confidence: **high**

```python
class PrimaryKeyConstraint(ColumnCollectionConstraint):
  __visit_name__ = "primary_key_constraint"
```
(`lib/sqlalchemy/sql/schema.py:5280–5344`)

- Always present on `Table`, even if implicit (auto-created from `primary_key=True` columns with `_implicit_generated=True`).
- Composite PKs are modeled the same as single-column PKs: multiple columns in the constraint's column collection.
- The `_implicit_generated` flag controls how naming conventions interact (naming event fires only for implicit PK constraints, `lib/sqlalchemy/sql/naming.py:172–186`).

**Djogi 0.1.0 context:** Djogi skips composite PKs for 0.1.0. In SQLAlchemy terms, the representation would be a `PrimaryKeyConstraint` with multiple columns in `.columns`. The `_implicit_generated=False` path (explicit `PrimaryKeyConstraint` in table definition) is how you name it explicitly.

### `UniqueConstraint`

Confidence: **high**

```python
class UniqueConstraint(ColumnCollectionConstraint):
  __visit_name__ = "unique_constraint"
```
(`lib/sqlalchemy/sql/schema.py:5525–5534`)

No additional attributes beyond the base. Composite unique constraints work identically to single-column: multiple columns in the column collection.

**Critical distinction from `Index(unique=True)`:** In Postgres, a `UniqueConstraint` is implemented via a unique index behind the scenes. During reflection, `PGDialect` detects this and sets `index["duplicates_constraint"] = index_name` on the reflected index entry (`lib/sqlalchemy/dialects/postgresql/base.py:5278–5279`). When table autoload is done, the unique index that backs a constraint is **not** returned in `Table.indexes` — it is represented only as the `UniqueConstraint` in `Table.constraints` (`lib/sqlalchemy/dialects/postgresql/base.py:1182–1193`).

**Djogi implication:** Djogi's differ must treat `UniqueConstraint` and `Index(unique=True)` as distinct representations. A user-facing `unique` field on a model generates a `UniqueConstraint` (constraint semantics: it has a name from the naming convention, it's deferrable). A separate `index` field generates an `Index`.

### `ForeignKeyConstraint`

Confidence: **high**

```python
class ForeignKeyConstraint(ColumnCollectionConstraint):
  __visit_name__ = "foreign_key_constraint"

  def __init__(
    self,
    columns,     # local column names
    refcolumns,    # "table.column" strings or Column objects
    name=None,
    onupdate=None,  # "CASCADE" | "SET NULL" | "RESTRICT" |...
    ondelete=None,  # same
    deferrable=None,
    initially=None,
    use_alter=False, # defer to ALTER TABLE (cycle-breaking)
    match=None,    # MATCH SIMPLE | PARTIAL | FULL
   ...
  )
```
(`lib/sqlalchemy/sql/schema.py:4983–5126`)

- `onupdate` and `ondelete` are free-form strings. SQLAlchemy does not validate them.
- `use_alter=True` causes the FK to be emitted as `ALTER TABLE... ADD CONSTRAINT` after all tables are created, enabling FK cycles to be resolved.
- Duplicate source columns (e.g., `FOREIGN KEY (a, a) REFERENCES r (b, c)`) raise `ArgumentError` at construction time.
- Internally creates `ForeignKey` element objects (one per column pair) which are attached to the `Column.foreign_keys` set.

DDL rendering (`lib/sqlalchemy/sql/compiler.py:7517–7528`):
```python
text = "FOREIGN KEY(%s) REFERENCES %s (%s)" % (
  ", ".join(preparer.quote(f.parent.name) for f in constraint.elements),
  self.define_constraint_remote_table(constraint, remote_table, preparer),
  ", ".join(preparer.quote(f.column.name) for f in constraint.elements),
)
```

### `CheckConstraint`

Confidence: **high**

```python
class CheckConstraint(ColumnCollectionConstraint):
  __visit_name__ = "table_or_column_check_constraint"

  def __init__(self, sqltext, name=None, deferrable=None, initially=None,...)
```
(`lib/sqlalchemy/sql/schema.py:4856–4927`)

`sqltext` is a string or SQL expression. If a string, it is wrapped in `text()`. Can be attached to a table or a column (column-level check constraints are supported). The `is_column_level` property distinguishes them.

Postgres DDL adds `NOT VALID` support via `_define_constraint_validity` (`lib/sqlalchemy/dialects/postgresql/base.py:2559–2561`): `constraint.dialect_options["postgresql"]["not_valid"]`. This is relevant for adding check constraints without validating existing rows.

### `Index`

Confidence: **high**

```python
class Index(DialectKWArgs, ColumnCollectionMixin, HasConditionalDDL, SchemaItem):
  __visit_name__ = "index"

  def __init__(
    self,
    name,
    *expressions,   # Column objects, SQL expressions, text()
    unique=False,
   ...
    **dialect_kw,   # postgresql_where, postgresql_using, postgresql_ops, etc.
  )
```
(`lib/sqlalchemy/sql/schema.py:5537–5685`)

`expressions` accepts column objects, arbitrary SQL expressions (for functional/expression indexes), and `text()` (for raw SQL expressions). The expressions are stored in `self.expressions` and resolved to table-bound forms in `_set_parent`.

Note: `Index` does **not** inherit from `Constraint`. It is a separate class. This means it is not in `Table.constraints` — it is in `Table.indexes`. (`lib/sqlalchemy/sql/schema.py:5699`)

---

## Naming conventions (`naming.py`)

Confidence: **high** (directly read from source)

**This is the single most important lesson for Djogi.** The naming convention system provides deterministic, collision-resistant constraint names derived from table and column names, without requiring the user to specify every name manually.

### Mechanism

`MetaData.naming_convention` is a dict mapping constraint types (or short mnemonics) to format strings. When a constraint is attached to a table (via the `after_parent_attach` event), `naming.py` fires and computes the name from the template.

```python
@event.listens_for(Constraint, "after_parent_attach")
@event.listens_for(Index, "after_parent_attach")
def _constraint_name(const, table):
  if isinstance(table, Table):
    newname = _constraint_name_for_table(const, table)
    if newname:
      const.name = newname
```
(`lib/sqlalchemy/sql/naming.py:188–209`)

The `_constraint_name_for_table` function applies the format string using `ConventionDict` as the format dict. `ConventionDict.__getitem__` resolves tokens by calling `_key_<token_name>()` methods.

### Short mnemonics

The five constraint type mnemonics (`lib/sqlalchemy/sql/naming.py:130–136`):

```python
_prefix_dict = {
  Index: "ix",
  PrimaryKeyConstraint: "pk",
  CheckConstraint: "ck",
  UniqueConstraint: "uq",
  ForeignKeyConstraint: "fk",
}
```

### Available tokens

From `MetaData.__init__` docstring (`lib/sqlalchemy/sql/schema.py:5878–5926`):

| Token | Meaning |
|---|---|
| `%(table_name)s` | Name of the table |
| `%(referred_table_name)s` | Target table for FK |
| `%(column_0_name)s` | Name of the first column |
| `%(column_0N_name)s` | All column names concatenated (no separator) |
| `%(column_0_N_name)s` | All column names joined with `_` |
| `%(column_0_key)s` | Key of the first column (same as name unless aliased) |
| `%(column_0N_key)s` | All column keys, no separator |
| `%(column_0_N_key)s` | All column keys, `_` separated |
| `%(column_0_label)s` | `_ddl_label` of the first column |
| `%(column_0N_label)s` | All labels, no separator |
| `%(column_0_N_label)s` | All labels, `_` separated |
| `%(referred_column_0_name)s` | FK target column name (first) |
| `%(referred_column_0N_name)s` | FK target column names, no separator |
| `%(referred_column_0_N_name)s` | FK target column names, `_` separated |
| `%(constraint_name)s` | The constraint's existing explicit name (requires constraint to already have a name) |

User-defined tokens are supported: the value in the naming_convention dict can be a callable `fn(constraint, table) -> str`.

### Multi-column token resolution (`ConventionDict.__getitem__`)

The `column_0_N_name` pattern (with `_N`) iterates all columns and joins with underscore. `column_0N_name` (no separator `_N`) joins without separator. These patterns extend to higher indices: `column_1_name`, `column_2_name`, etc. for accessing specific columns by position. The implementation uses regex matching on the token name: `re.match(r".*_?column_(\d+)(_?N)?_.+", key)`. (`lib/sqlalchemy/sql/naming.py:103–127`)

### Default convention (SQLAlchemy-recommended)

The default is only:

```python
DEFAULT_NAMING_CONVENTION = util.immutabledict({"ix": "ix_%(column_0_label)s"})
```
(`lib/sqlalchemy/sql/schema.py:5785–5787`)

The commonly-recommended fuller convention (from SQLAlchemy docs) is:

```python
convention = {
  "ix": "ix_%(column_0_label)s",
  "uq": "uq_%(table_name)s_%(column_0_N_name)s",
  "ck": "ck_%(table_name)s_%(constraint_name)s",
  "fk": "fk_%(table_name)s_%(column_0_N_name)s_%(referred_table_name)s",
  "pk": "pk_%(table_name)s",
}
```

This is **not** built in — it must be passed to `MetaData(naming_convention=convention)`. The default only auto-names indexes.

### Why this matters for Djogi's differ

Alembic's autogenerate compares constraint names to detect add/remove/modify. If names are non-deterministic (or `None`), Alembic cannot reliably match constraints between metadata and database state. Djogi faces the exact same problem: the differ needs to match composite unique constraints and FK constraints by name across the descriptor and the live schema.

**Without a naming convention, constraints may have `None` names, making diffing impossible for unnamed composite constraints.**

The `%(column_0_N_name)s` token is the critical one for Djogi: it encodes all columns of a composite constraint in the name, providing collision resistance for most real-world schemas. A table with two different composite unique constraints on different column sets gets different names.

---

## DDL generation

### Base `DDLCompiler`

Confidence: **high**

`DDLCompiler` extends `Compiled` and lives in `lib/sqlalchemy/sql/compiler.py:6990`. It is initialized with a `Dialect` and a `BaseDDLElement` (the DDL construct to render). DDL caching is disabled (`_hierarchy_supports_caching = False`, `lib/sqlalchemy/sql/ddl.py:80`).

**`visit_create_table`** (`lib/sqlalchemy/sql/compiler.py:7056–7106`):

```python
text = "\nCREATE "
if table._prefixes:
  text += " ".join(table._prefixes) + " "
text += "TABLE "
...
text += preparer.format_table(table) + " "
...
text += "("
for create_column in create.columns:
 ...
const = self.create_table_constraints(table,...)
if const:
  text += separator + "\t" + const
text += "\n)%s\n\n" % self.post_create_table(table)
```

The `create_table_constraints` method orders constraints: PK first, then FKs, then all others.

**`visit_create_index`** (`lib/sqlalchemy/sql/compiler.py:7224–7253`):

```python
text = "CREATE "
if index.unique:
  text += "UNIQUE "
text += "INDEX "
...
text += "%s ON %s (%s)" % (
  self._prepared_index_name(index,...),
  preparer.format_table(index.table,...),
  ", ".join(
    self.sql_compiler.process(expr, include_table=False, literal_binds=True)
    for expr in index.expressions
  ),
)
```

**Constraint name rendering** (`lib/sqlalchemy/sql/compiler.py:7487–7495`):

```python
def define_constraint_preamble(self, constraint, **kw):
  text = ""
  if constraint.name is not None:
    formatted_name = self.preparer.format_constraint(constraint)
    if formatted_name is not None:
      text += "CONSTRAINT %s " % formatted_name
  return text
```

A `None` constraint name silently produces no `CONSTRAINT name` clause — the constraint is anonymous.

### Postgres-specific DDL (`PGDDLCompiler`)

Confidence: **high**

Located at `lib/sqlalchemy/dialects/postgresql/base.py:2504`.

**Column specification** adds SERIAL/BIGSERIAL/SMALLSERIAL for auto-increment columns:

```python
if isinstance(impl_type, sqltypes.BigInteger):
  colspec += " BIGSERIAL"
elif isinstance(impl_type, sqltypes.SmallInteger):
  colspec += " SMALLSERIAL"
else:
  colspec += " SERIAL"
```
(`lib/sqlalchemy/dialects/postgresql/base.py:2532–2537`)

**`visit_create_index`** (PG override, `lib/sqlalchemy/dialects/postgresql/base.py:2675–2764`):

- Reads `index.dialect_options["postgresql"]["concurrently"]` → emits `CONCURRENTLY`
- Reads `index.dialect_options["postgresql"]["using"]` → emits `USING <access_method>`
- Reads `index.dialect_options["postgresql"]["ops"]` → emits operator classes per column
- Reads `index.dialect_options["postgresql"]["include"]` (via `_define_include`) → emits `INCLUDE (...)`
- Reads `index.dialect_options["postgresql"]["nulls_not_distinct"]` → emits `NULLS NOT DISTINCT` / `NULLS DISTINCT`
- Reads `index.dialect_options["postgresql"]["where"]` → emits `WHERE <predicate>`

Full rendered form for a hypothetical partial GIN index with opclass and INCLUDE:
```sql
CREATE INDEX CONCURRENTLY ix_foo ON bar USING gin (col text_pattern_ops) INCLUDE (extra_col) WHERE (col IS NOT NULL)
```

**`visit_unique_constraint`** (PG override, `lib/sqlalchemy/dialects/postgresql/base.py:2605–2611`):

```python
def visit_unique_constraint(self, constraint, **kw):
  if len(constraint) == 0:
    return ""
  text = self.define_constraint_preamble(constraint, **kw)
  text += self.define_unique_body(constraint, **kw)
  text += self._define_include(constraint)  # INCLUDE support
  text += self.define_constraint_deferrability(constraint)
  return text
```

PG unique constraints can also have `INCLUDE` columns (covering unique constraints). The PK constraint similarly has `_define_include` added (`lib/sqlalchemy/dialects/postgresql/base.py:2598–2603`).

**`post_create_table`** (`lib/sqlalchemy/dialects/postgresql/base.py:2824–2863`): adds table-level PG options: `INHERITS`, `PARTITION BY`, `USING` (access method), `WITH` (storage parameters), `ON COMMIT`, `TABLESPACE`.

---

## Reflection (introspection)

### `Inspector` API

Confidence: **high**

```python
from sqlalchemy import inspect
insp = inspect(engine)
insp.get_table_names(schema=None)
insp.get_columns(table_name, schema=None)
insp.get_pk_constraint(table_name, schema=None)
insp.get_foreign_keys(table_name, schema=None)
insp.get_indexes(table_name, schema=None)
insp.get_unique_constraints(table_name, schema=None)
insp.get_check_constraints(table_name, schema=None)
```

(`lib/sqlalchemy/engine/reflection.py:182–1387`)

### Postgres-specific introspection

Confidence: **high**

The PG dialect introspects via the `pg_catalog` tables, modeled as SQLAlchemy `Table` objects in `lib/sqlalchemy/dialects/postgresql/pg_catalog.py`. Key tables: `pg_class`, `pg_namespace`, `pg_attribute`, `pg_index`, `pg_constraint`, `pg_type`, `pg_sequence`.

**Columns query** (`lib/sqlalchemy/dialects/postgresql/base.py:4122–4185`):

Joins `pg_attribute` with `pg_class`, `pg_namespace`. Uses `pg_catalog.format_type()` for type names, `pg_catalog.pg_get_expr()` for default expressions, and conditional logic on `pg_attribute.attgenerated` (>= PG 12) and `pg_attribute.attidentity` (>= PG 10) to detect generated columns and identity columns respectively.

**Index query** (`lib/sqlalchemy/dialects/postgresql/base.py:5007–5158`): Complex multi-subquery approach:

1. `idx_sq`: Unnests `pg_index.indkey` (column positions) and `pg_index.indclass` (operator class OIDs), ordered by position.
2. `attr_sq`: Joins with `pg_attribute`; for expression index elements (position == 0), calls `pg_get_indexdef(indexrelid, ord+1, true)` to get the expression text.
3. `cols_sq`: Aggregates column names/expressions and opclass OIDs in order using `array_agg(... ORDER BY ord)`.
4. Final SELECT joins with `pg_class` (index name), `pg_constraint` (to detect if index backs a constraint), reads `indpred` via `pg_get_expr` (partial index predicate), `indoption` (column sort options), `reloptions` (storage params), `relam` (access method OID), `indnullsnotdistinct` (>= PG 15).

This query returns `filter_definition` (the partial index `WHERE` clause text) and `has_constraint` (whether the index backs a constraint). When `has_constraint=True`, the reflected index entry gets `"duplicates_constraint": index_name` (`lib/sqlalchemy/dialects/postgresql/base.py:5278–5279`).

**Unique constraints query**: Delegates to `_reflect_constraint(connection, "u",...)` — queries `pg_constraint` filtering on `contype = 'u'`. (`lib/sqlalchemy/dialects/postgresql/base.py:5335–5367`)

### How Alembic's autogenerate uses this

Alembic calls `Inspector` to build a "conn_metadata" (live schema state) and then diffs it against the user-supplied "metadata" (in-code descriptor). The diff is done object-by-object: tables, columns, indexes, constraints. Server defaults are compared separately in `alembic/autogenerate/compare/server_defaults.py`.

### Djogi relevance

If Djogi ever needs `inspectdb`-style baseline adoption (introspect a live database and generate Rust model definitions), the Postgres `pg_catalog` query patterns in `PGDialect` are the reference implementation. The multi-subquery index query (`pg_index` + `pg_attribute` + `pg_get_indexdef`) correctly handles expression indexes, opclasses, partial index predicates, and INCLUDE columns.

---

## Postgres-specific features

### Type tour

Confidence: **high** for declaration; **high** for rendering (directly read PGTypeCompiler)

| Type | Python class | DDL keyword | Module |
|---|---|---|---|
| UUID | `PGUuid` (via `sqltypes.UUID`) | `UUID` | `types.py` |
| JSONB | `JSONB` | `JSONB` | `json.py` |
| JSON | `JSON` | `JSON` | `json.py` |
| INET | `INET` | `INET` | `types.py` |
| CIDR | `CIDR` | `CIDR` | `types.py` |
| CITEXT | `CITEXT` (subclass of TEXT) | `CITEXT` | `types.py` |
| TSVECTOR | `TSVECTOR` | `TSVECTOR` | `types.py` |
| HSTORE | `HSTORE` | `HSTORE` | `hstore.py` |
| ARRAY | `ARRAY` | `item_type[]` | `array.py` |
| ENUM | `ENUM` (named type) | `CREATE TYPE... AS ENUM` | `named_types.py` |
| Ranges | `INT4RANGE`, `TSTZRANGE`, etc. | `INT4RANGE`, etc. | `ranges.py` |

### Index modifiers (`postgresql_*` dialect kwargs)

Confidence: **high** (verified in `PGDDLCompiler.visit_create_index`)

All passed as `**dialect_kw` to `Index(...)`:

- `postgresql_using="gin"` → `USING gin` (`lib/sqlalchemy/dialects/postgresql/base.py:2698–2703`)
- `postgresql_ops={"col": "text_pattern_ops"}` → operator class per column (`lib/sqlalchemy/dialects/postgresql/base.py:2705–2726`)
- `postgresql_include=["col"]` → `INCLUDE (col)` covering index (`lib/sqlalchemy/dialects/postgresql/base.py:2563–2573`, `2728`)
- `postgresql_where=<expression>` → partial index `WHERE` clause (`lib/sqlalchemy/dialects/postgresql/base.py:2753–2762`)
- `postgresql_concurrently=True` → `CREATE INDEX CONCURRENTLY` (`lib/sqlalchemy/dialects/postgresql/base.py:2685–2688`)
- `postgresql_nulls_not_distinct=True` → `NULLS NOT DISTINCT` for unique indexes (`lib/sqlalchemy/dialects/postgresql/base.py:2730–2736`)

For `CONCURRENTLY` on DROP: same `postgresql_concurrently` flag, read in `visit_drop_index` (`lib/sqlalchemy/dialects/postgresql/base.py:2783–2786`). The dialect tracks `_supports_create_index_concurrently` and `_supports_drop_index_concurrently` as class flags both defaulting `True` (`lib/sqlalchemy/dialects/postgresql/base.py:3526–3527`).

**Partial indexes:**

```python
Index("my_index", my_table.c.id, postgresql_where=my_table.c.value > 10)
```

The `where` clause is compiled using the SQL compiler with `include_table=False, literal_binds=True`.

**Expression (functional) indexes:**

```python
Index("some_index", func.lower(sometable.c.name))
# or
Index("some_index", text("lower(name)"))
```

(`lib/sqlalchemy/sql/schema.py:5568–5598`)

Expression indexes work by passing non-`Column` `ClauseElement` objects as index expressions. During reflection, `attnum == 0` in `pg_index.indkey` signals an expression index element, and `pg_get_indexdef(indexrelid, ord+1, true)` retrieves the expression text.

### Enum handling

Confidence: **high** for the lifecycle; **medium** for migration edge cases (Alembic-side)

Postgres `ENUM` is a named type (`CREATE TYPE name AS ENUM ('a', 'b', 'c')`). SQLAlchemy's `ENUM` class (in `lib/sqlalchemy/dialects/postgresql/named_types.py:192`) is a `NamedType` — it has its own `create()` and `drop()` methods separate from table DDL.

Key behaviors:

1. **On `Table.create()`**: If the ENUM type does not exist, `CREATE TYPE` is emitted before `CREATE TABLE`. (`lib/sqlalchemy/dialects/postgresql/named_types.py:208–218`)
2. **On `Table.drop()`**: As of SQLAlchemy 2.1, the ENUM is **not** dropped when a single table is dropped, because the type may be shared across tables. (`lib/sqlalchemy/dialects/postgresql/named_types.py:253–261`)
3. **On `MetaData.drop_all()`**: All associated types are dropped.
4. **Deduplication**: `_check_for_name_in_memos` tracks type names to avoid emitting `CREATE TYPE` twice during a `create_all` run. (`lib/sqlalchemy/dialects/postgresql/named_types.py:74–98`)

**Migration-hostile aspects:**

- **Adding a value** to an existing ENUM: Postgres 10+ supports `ALTER TYPE... ADD VALUE`. This is non-transactional — you cannot run it inside a transaction and have it visible within the same transaction. Alembic provides `op.execute("ALTER TYPE...")` as the escape hatch; it does not generate this from autogenerate.
- **Removing a value**: Not supported by Postgres at all. Requires `CREATE TYPE... AS ENUM (new_values)`, then `ALTER TABLE... ALTER COLUMN... TYPE new_type USING col::text::new_type`, then `DROP TYPE old_type`. This must be done manually.
- **Renaming a value**: `ALTER TYPE... RENAME VALUE` (PG 10+), also non-transactional.
- **Alembic autogenerate limitation**: Alembic does not detect ENUM value additions/removals in autogenerate. It only detects type-level changes (column type change from one ENUM to another). This means enum value drift accumulates silently unless the user manually adds the migration.

**Djogi implication:** Djogi should represent ENUM as a named database type with explicit values in the descriptor. Migration generation should detect ENUM value changes. Value removals and renames must be flagged as requiring manual SQL (they are potentially breaking changes). Value additions should emit `ALTER TYPE... ADD VALUE` with a note that it cannot run inside a transaction.

---

## Server defaults and generated columns

### `server_default`

Confidence: **high**

`Column.server_default` holds a `FetchedValue` instance. The concrete types:

- `DefaultClause(arg)` — wraps a string or SQL expression; emits `DEFAULT <value>` in DDL
- `FetchedValue()` — marks the column as server-managed but emits no DEFAULT in DDL (used when a trigger or implicit sequence provides the value)
- `Computed(sqltext, persisted=None)` — generated column (`GENERATED ALWAYS AS...`)
- `Identity(...)` — `GENERATED ALWAYS AS IDENTITY` / `GENERATED BY DEFAULT AS IDENTITY`

(`lib/sqlalchemy/sql/schema.py:4355–4541`, `4416–4449`, `6418–6521`, `6524`)

When `Column(server_default="val")` is used, the string is wrapped in `DefaultClause` at construction time (`lib/sqlalchemy/sql/schema.py:2421–2427`).

### `Computed` (generated columns)

Confidence: **high**

```python
Column("area", Float, Computed("side * side"))
```

The `Computed` object stores `sqltext` and `persisted`. In Postgres DDL, `PGDDLCompiler.visit_computed_column` renders `GENERATED ALWAYS AS (%s) STORED` for PG versions before 18; PG 18+ will support VIRTUAL as default. (`lib/sqlalchemy/dialects/postgresql/base.py:2865–2887`)

`Computed` sets itself as both `column.server_default` and `column.server_onupdate`. This means autogenerate must special-case `Computed` columns.

### Autogenerate noise from `server_default`

Confidence: **high** (from Alembic source)

Server defaults are notoriously noisy in Alembic autogenerate because:

1. Databases may normalize default expressions: `'50'::integer` in catalog vs `'50'` in metadata.
2. There is no canonical text form for SQL expressions. `now()`, `NOW()`, `current_timestamp` may all be equivalent depending on Postgres version.
3. Alembic's `_render_server_default_for_compare` (`alembic-reference/alembic/autogenerate/compare/server_defaults.py:32–48`) compiles the metadata default to a string and compares it with the reflected default string. String-equality comparison means any whitespace or casing difference triggers a false diff.
4. `Computed` columns cannot be modified via `ALTER COLUMN` — Alembic emits a warning rather than a migration op. (`alembic-reference/alembic/autogenerate/compare/server_defaults.py:119–120`)

**Djogi implication:** Djogi's differ should allow explicit opt-out for `server_default` comparison (similar to Alembic's `compare_server_default=False`). For generated columns, the differ should detect them and warn/block rather than emitting broken migration SQL.

---

## Extensions and operator classes

Confidence: **medium** (extension handling not deeply tested; verified by absence of relevant DDL generation code)

SQLAlchemy does **not** track or manage Postgres extension dependencies. CITEXT, HSTORE, pg_trgm, uuid-ossp, and similar extensions must be installed manually by the user or via a startup hook. There is no `CREATE EXTENSION` DDL generation in the schema metadata layer.

Evidence: The only `CREATE EXTENSION` reference in the production code is in `lib/sqlalchemy/dialects/postgresql/provision.py:182` — a test provisioning helper that runs `CREATE EXTENSION IF NOT EXISTS citext/hstore` when setting up test databases. This file is explicitly test infrastructure, not production schema machinery.

The `ColumnCollectionMixin` and `DialectKWArgs` pattern does allow user code to pass arbitrary `postgresql_*` kwargs to `Index` (for opclass usage), but these are index-level, not extension-level.

**Djogi implication:** Djogi should similarly treat extensions as out-of-band. The descriptor can reference CITEXT, JSONB, etc. but Djogi should not auto-emit `CREATE EXTENSION`. Instead, a validation step on migration generation could warn if the required extension is not installed.

---

## Lessons for Djogi

### Adopt

1. **The naming convention system.** This is the single most important takeaway. Djogi's descriptor-driven differ needs deterministic, stable names for all composite constraints and indexes. Without them, the differ cannot match constraints between the descriptor and the live schema. Adopt the token system exactly. The five prefixes (`ix`, `uq`, `ck`, `fk`, `pk`) and the `%(table_name)s_%(column_0_N_name)s` pattern are the minimum viable set. The `%(column_0_N_name)s` token (underscore-joined all column names) is load-bearing for composite unique constraints and FK constraints.

2. **Topological sort with `use_alter` escape hatch.** `MetaData.sorted_tables` + `sort_tables_and_constraints` is the right pattern. Djogi's migration plan must order table creates by FK dependency. For cycles, Djogi needs a `defer_fk` equivalent that defers FK constraint creation to post-all-tables.

3. **`has_constraint` / `duplicates_constraint` in index reflection.** When reflecting, Postgres returns the unique index that backs a `UNIQUE CONSTRAINT` alongside the constraint itself. Alembic/SQLAlchemy deduplicate them by the `has_constraint` flag. Djogi must do the same: a `UniqueConstraint` and its backing index are one logical entity.

4. **`deferrable` and `initially` as first-class constraint attributes.** These are standard SQL and SQLAlchemy puts them on the base `Constraint`. Djogi should include them from the start rather than treating them as a Postgres extension.

5. **`NOT VALID` for check and FK constraints.** SQLAlchemy exposes this as `postgresql_not_valid=True` on the constraint. Djogi's migration planner should support this — especially relevant for adding check constraints to large tables without a full table scan.

6. **Separate `server_default` from `Computed`.** These are different concepts in the descriptor. A `server_default` is a default value; a generated column (`Computed`) is a formula. They require different DDL and different differ behavior (generated columns cannot be ALTERed, only dropped and re-added).

7. **The `pg_catalog` query patterns for reflection.** The index query (using `unnest(indkey)` + `generate_subscripts` + `pg_get_indexdef` for expression elements) is battle-tested. If Djogi needs `inspectdb`, this is the reference.

### Reject

1. **The late-binding architecture** (string column references resolved at table-attach time). SQLAlchemy supports `ForeignKey("other_table.id")` as a string that resolves lazily. This creates complexity in `_fk_memos` and the event system. Djogi's Rust descriptor is fully static — all references resolve at macro expansion time. This simplicity is an advantage; do not add lazy string resolution.

2. **Anonymous constraint names as a valid state.** SQLAlchemy allows constraints with `name=None` silently. This is the root cause of autogenerate noise and migration instability. Djogi should require names on all composite constraints (generated by the naming convention system if not explicitly set). Single-column anonymous constraints are acceptable if they generate a stable name.

3. **`TypeDecorator` complexity for migration purposes.** `TypeDecorator` adds Python-side processing on top of types. For migration DDL, only the underlying `impl` type matters. Djogi's differ should unwrap `TypeDecorator` to get the DDL type.

4. **The `ColumnDefault` / `FetchedValue` hierarchy** for client-side defaults. Client-side `default=` and `onupdate=` are ORM concerns, not DDL concerns. Djogi's descriptor should only model `server_default` (a `DefaultClause` equivalent) and not attempt to model Python-side defaults in migration metadata.

### Defer

1. **Full reflection / `inspectdb`-style baseline adoption.** The `pg_catalog` queries are complex (especially the index query). Defer this to post-0.1.0. For now, require users to write the initial migration by hand if adopting an existing database.

2. **Partial index and expression index DDL in the differ.** The differ for 0.1.0 can treat expression indexes as opaque (detect add/remove but not modify). Detailed semantic comparison of WHERE clauses and expressions is deferred.

3. **ENUM value diffing.** Detecting ENUM value additions and removals requires special handling (non-transactional DDL, Postgres limitations). Defer to post-0.1.0; for 0.1.0, ENUM changes require manual migration.

4. **`CONCURRENTLY` index creation.** Building indexes concurrently is Postgres-specific and requires not wrapping the statement in a transaction. Defer, but design the migration runner to support non-transactional statement execution early so this can be added without architecture changes.

5. **`ExclusionConstraint`.** SQLAlchemy supports `EXCLUDE USING` via `ExclusionConstraint` in `lib/sqlalchemy/dialects/postgresql/ext.py`. This is niche; defer.

### Surprises

1. **The default naming convention only covers indexes, not constraints.** `DEFAULT_NAMING_CONVENTION = {"ix": "ix_%(column_0_label)s"}`. FK, unique, check, and PK constraints have no default auto-naming. This means a vanilla SQLAlchemy project without an explicit `naming_convention` has anonymous composite constraints — and Alembic's autogenerate cannot reliably diff them by name. This is a known pain point in the ecosystem. Djogi should default to a full naming convention for all constraint types.

2. **`UniqueConstraint` and `Index(unique=True)` are not the same object.** In Postgres, a `UNIQUE CONSTRAINT` is implemented as a unique index, but they are modeled differently in SQLAlchemy. The constraint appears in `Table.constraints`; the backing index appears in `get_indexes()` with `duplicates_constraint` set. If Djogi's model uses a `unique` field on a column/table, it should generate a `UniqueConstraint` (with a name), not an anonymous `Index(unique=True)`.

3. **`CONCURRENTLY` cannot run inside a transaction.** SQLAlchemy signals this only through documentation; there is no enforcement in the DDL layer. The migration runner must handle this. The flag `_supports_create_index_concurrently=True` is on the dialect class (`lib/sqlalchemy/dialects/postgresql/base.py:3526`), meaning it can be disabled per-dialect subclass.

4. **PG `ENUM` is a schema-level named type, not a column-level type.** It must be `CREATE TYPE`d before the table and `DROP TYPE`d separately. It can be shared across tables. This makes ENUM migrations stateful in a way that other column types are not. The `NamedType._check_for_name_in_memos` deduplication mechanism (`lib/sqlalchemy/dialects/postgresql/named_types.py:74–98`) is the right pattern for ensuring types are only created once per migration run.

5. **`server_default` text comparison is fragile.** Alembic compares the rendered default string from metadata with the reflected default string from `pg_attribute.adbin` / `pg_get_expr()`. The same semantic expression can have multiple syntactic forms in Postgres. This means `server_default` comparisons produce false positives unless the user carefully mirrors the Postgres canonical form. Djogi's differ should document this limitation and default to not comparing server defaults unless explicitly opted in.

---

## Confidence

| Section | Confidence | Notes |
|---|---|---|
| MetaData / Table / Column attributes | high | Directly read from schema.py |
| Naming convention token system | high | Directly read from naming.py and schema.py |
| DDLCompiler visitor methods | high | Directly read from compiler.py |
| PGDDLCompiler index DDL | high | Directly read from base.py |
| Constraint deferrable/initially | high | Directly read |
| PG index reflection query | high | Directly read from base.py |
| ENUM migration behavior | high (lifecycle) / medium (migration edge cases) | Edge cases require Alembic testing |
| server_default autogenerate noise | high | Cross-verified with Alembic compare source |
| Extension handling | medium | Confirmed by absence of CREATE EXTENSION in prod code |
| Default SQLAlchemy-recommended naming convention string | medium | The exact recommended string is in docs; verified that DEFAULT_NAMING_CONVENTION only has `ix` |
| CONCURRENTLY non-transactional constraint | high | Documentation + flags in source |

### Parts not fully read

- `lib/sqlalchemy/sql/sqltypes.py` — generic type system, not read in detail
- `lib/sqlalchemy/dialects/postgresql/ranges.py` — range types, not read
- `lib/sqlalchemy/dialects/postgresql/ext.py` — `ExclusionConstraint`, partial read
- `lib/sqlalchemy/sql/ddl.py` `SchemaGenerator`/`SchemaDropper` bodies — confirmed structure, did not read every method
- Alembic autogenerate for indexes and constraints (`compare/constraints.py`, `compare/types.py`) — not read in this session

---

## Open questions for synthesis

1. **Cross-project naming convention comparison.** Django uses `%(app_label)s_%(class_name)s` style names generated by the ORM. Prisma generates hash-based names. Does any system other than SQLAlchemy use column-content-based names for constraints? Which approach gives the best stability under table renames?

2. **ENUM across migration systems.** How do Django and Prisma handle ENUM value additions and removals? Is the Alembic "do it manually" approach standard, or do some systems have transactional workarounds?

3. **`use_alter` / deferred FK equivalent in other systems.** How do Django and Flyway handle FK cycles during `CREATE TABLE`? Do they have a `defer FK` mechanism or do they just rely on CREATE ordering?

4. **Partial index / expression index diffing.** Is there any migration system that meaningfully diffs partial index WHERE clauses semantically (rather than text equality)? Semantic equality of SQL expressions is undecidable in general but bounded cases may be tractable.

5. **`CONCURRENTLY` transaction isolation.** Prisma and Diesel both claim online-safe migration support. What patterns do they use for index creation to avoid locking? Do they emit `CREATE INDEX CONCURRENTLY` automatically?

6. **Server default normalization.** The SQLAlchemy/Alembic fragility around server default text comparison is a known wart. Is there a system that handles this better (e.g., by introspecting at a semantic level rather than text)? This seems like an area Djogi can improve on from day one by normalizing default expressions before comparison.
