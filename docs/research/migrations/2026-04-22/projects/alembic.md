# Alembic

## Metadata
- Clone path: `/home/tarunvir/projects/alembic-reference/`
- Commit SHA inspected: `0ab90276fc583d52e31e95d3f59b4b6c00ec39ee`
- Primary language: Python
- Migration-relevant modules:
 - `alembic/command.py` — top-level CLI verbs (upgrade, downgrade, stamp, merge, revision, check)
 - `alembic/runtime/migration.py` — `MigrationContext`, `HeadMaintainer`, `RevisionStep`, `StampStep`
 - `alembic/runtime/environment.py` — `EnvironmentContext`, all `configure()` options
 - `alembic/ddl/impl.py` — `DefaultImpl`, `version_table_impl()`
 - `alembic/ddl/postgresql.py` — `PostgresqlImpl` (transactional_ddl)
 - `alembic/script/revision.py` — `RevisionMap`, `Revision`, DAG traversal
 - `alembic/script/base.py` — `ScriptDirectory`, `Script`, file generation
 - `alembic/autogenerate/api.py` — `AutogenContext`, `compare_metadata()`, `produce_migrations()`
 - `alembic/autogenerate/compare/__init__.py` — `comparators` registry, `_populate_migration_script()`
 - `alembic/autogenerate/compare/tables.py` — table/column diff
 - `alembic/autogenerate/compare/constraints.py` — indexes, unique constraints, FK diff
 - `alembic/autogenerate/compare/types.py` — type comparison dispatch
 - `alembic/autogenerate/compare/server_defaults.py` — server default comparison
 - `alembic/autogenerate/compare/schema.py` — schema-level orchestration
 - `alembic/autogenerate/render.py` — Python code emission from ops
 - `alembic/operations/ops.py` — operation classes (`CreateTableOp`, `AlterColumnOp`, etc.)
 - `alembic/operations/batch.py` — `BatchOperationsImpl`, `ApplyBatchImpl` (copy-modify-swap)
 - `alembic/util/langhelpers.py` — `rev_id()` generation
 - `alembic/templates/generic/env.py` — canonical `env.py` template
- Approximate LOC of migration-relevant code: ~21,000 (sum of the above modules as measured by `wc -l`)

---

## Architecture

### Directory layout of `alembic/`

```
alembic/
 command.py     CLI entry points; thin wrappers over EnvironmentContext
 config.py      Config object (alembic.ini / pyproject.toml parsing)
 context.py     Module-level proxy to EnvironmentContext (the `alembic.context` object)
 environment.py   (root-level shim) re-exports runtime/environment
 migration.py    (root-level shim) re-exports runtime/migration
 op.py        Module-level proxy to Operations
 ddl/
  impl.py      DefaultImpl base: DDL execution, version_table_impl()
  postgresql.py   PostgresqlImpl (transactional_ddl=True, compare_type, batch prep)
  sqlite.py     SQLiteImpl (requires_recreate_in_batch)
  mysql.py, mssql.py, oracle.py
 runtime/
  migration.py   MigrationContext, HeadMaintainer, RevisionStep, StampStep
  environment.py  EnvironmentContext, all configure() hooks
  plugins.py    Plugin system (autogenerate comparator dispatch)
 script/
  revision.py    RevisionMap, Revision, DAG logic (branches, merges, traversal)
  base.py      ScriptDirectory, Script — file scanning, generate_revision()
  write_hooks.py  Post-write hooks (e.g. black formatting)
 autogenerate/
  api.py      AutogenContext, compare_metadata(), produce_migrations(), RevisionContext
  render.py     Python code generation from operation objects
  rewriter.py    MigrationRewriter (post-process script AST)
  compare/
   __init__.py   comparators registry (PriorityDispatcher), _populate_migration_script()
   schema.py    schema-level fan-out to table comparator
   tables.py    _autogen_for_tables(), _compare_tables(), _compare_columns()
   constraints.py _compare_indexes_and_uniques(), _compare_foreign_keys()
   types.py    _user_compare_type(), _dialect_impl_compare_type()
   server_defaults.py _compare_server_default(), _compare_computed_default()
   comments.py   table/column comment comparison
   util.py     _InspectorConv (caching inspector wrapper)
 operations/
  ops.py      All operation classes (2918 LOC); CreateTableOp, AlterColumnOp, etc.
  batch.py     BatchOperationsImpl, ApplyBatchImpl (copy-modify-swap for SQLite)
  base.py      Operations, BatchOperations base classes
  toimpl.py     Operation -> DDL dispatch
  schemaobj.py   schema object helpers
 templates/
  generic/env.py  canonical env.py template
  multidb/     multi-database env.py variant
  async/      asyncio variant
```

### Key relationships

- **ScriptDirectory** scans a `versions/` directory, builds a **RevisionMap** DAG from each `.py` file's `revision` / `down_revision` attributes.
- **command.py** functions call `script.run_env()` inside an `EnvironmentContext`, which loads `env.py`. The user's `env.py` calls `context.configure()` and `context.run_migrations()`.
- `run_migrations()` calls the `fn` closure set by the command, which yields `RevisionStep` objects. The `HeadMaintainer` serialises each step's effect on `alembic_version`.
- **autogenerate** runs inside the same `env.py` path: `AutogenContext` wires together `MigrationContext`, target `MetaData`, and a `PriorityDispatcher` of comparators. The dispatcher produces `UpgradeOps`, which `render.py` converts to Python source.

---

## State model (source-of-truth)

### Revision graph as DAG

The revision graph is a directed acyclic graph, not a linear sequence. Each `Revision` stores:
- `revision: str` — the node identity (random 12-char hex)
- `down_revision: Optional[_RevIdType]` — parent(s); a tuple for merge points
- `branch_labels: Set[str]` — optional symbolic labels propagated to ancestors
- `dependencies: Optional[_RevIdType]` — cross-branch ordering dependencies (not the same as down_revision for version table purposes)

`script/revision.py:1538-1657` (class `Revision`)

A **branch point** is a node with `len(self.nextrev) > 1` — i.e., more than one child revision names it as its `down_revision` (`script/revision.py:1679-1688`).

A **merge point** is a node with `len(self._versioned_down_revisions) > 1` — i.e., its own `down_revision` is a tuple of two-or-more parents (`script/revision.py:1699-1702`).

### `alembic_version` table role

The table stores only the **currently active head revision identifiers**. It is not a historical log. When the database is on a single linear head, there is exactly one row. When multiple unmerged branches are active, there is one row per branch head.

### Multiple heads

`MigrationContext.get_current_heads()` returns a tuple of all `version_num` values:

```python
return tuple(
  row[0]
  for row in self.connection.execute(
    select(self._version.c.version_num)
  )
)
```

`runtime/migration.py:536-542`

`get_current_revision()` calls `get_current_heads()` and raises `CommandError` if more than one is returned (`runtime/migration.py:472-497`).

### Revision identifiers

Generated by `uuid.uuid4().hex[-12:]` — the last 12 hex characters of a UUID4. Example: `a1b2c3d4e5f6`.

`alembic/util/langhelpers.py:231-232`

No sequential numbering. Filenames follow `file_template`, defaulting to `%(rev)s_%(slug)s.py` (`script/base.py:50`).

---

## Ledger / history table

### Exact DDL (SQLAlchemy Table object, source-of-truth)

The table is created in Python via SQLAlchemy's schema API, not as raw SQL. The canonical definition is in `alembic/ddl/impl.py:151-183`:

```python
def version_table_impl(
  self,
  *,
  version_table: str,
  version_table_schema: Optional[str],
  version_table_pk: bool,
  **kw: Any,
) -> Table:
  vt = Table(
    version_table,
    MetaData(),
    Column("version_num", String(32), nullable=False),
    schema=version_table_schema,
  )
  if version_table_pk:
    vt.append_constraint(
      PrimaryKeyConstraint(
        "version_num", name=f"{version_table}_pkc"
      )
    )

  return vt
```

`alembic/ddl/impl.py:151-183`

The equivalent DDL for PostgreSQL (with `version_table_pk=True`, the default) is:

```sql
CREATE TABLE alembic_version (
  version_num VARCHAR(32) NOT NULL,
  CONSTRAINT alembic_version_pkc PRIMARY KEY (version_num)
);
```

**Confidence: high** — read directly from source.

### Column purposes

- `version_num VARCHAR(32) NOT NULL` — stores exactly one revision identifier per row (the last 12 hex chars of a UUID4, so 12 characters, but the column is sized 32 for safety).

### Multiple heads representation

One row is inserted per active branch head. `HeadMaintainer._insert_version()` (`runtime/migration.py:714-722`) does `INSERT INTO alembic_version (version_num) VALUES ('<rev>')`. When a merge migration runs, extra rows are deleted and one is updated (`HeadMaintainer.update_to_step()`, `runtime/migration.py:774-817`).

### Primary key / constraints

Single-column primary key on `version_num` with constraint name `{version_table}_pkc`. This is opt-out: `version_table_pk=True` is the default (`runtime/environment.py:540-544`). Setting `version_table_pk=False` removes the PK — preserved only for backwards compatibility.

**What is absent:** no `applied_at` timestamp, no `execution_time`, no `checksum`, no `description`, no `success/failure` flag. The table is purely a set of active head identifiers.

---

## Execution

### Upgrade / downgrade semantics

`command.upgrade()` (`command.py:449-490`) resolves `script._upgrade_revs(revision, rev)` which calls `self.iterate_revisions(destination, current_rev, implicit_base=True)` and wraps each result in a `MigrationStep.upgrade_from_script()` (`script/base.py:406-422`). The steps are iterated in `MigrationContext.run_migrations()` (`runtime/migration.py:574-644`).

`command.downgrade()` (`command.py:493-537`) similarly calls `script._downgrade_revs(revision, rev)`. A range (`<from>:<to>`) is required for `--sql` mode (`command.py:519-524`).

Each `RevisionStep` holds a `migration_fn` pointing to the `upgrade` or `downgrade` function in the script module (`runtime/migration.py:1009-1012`).

### Transaction boundaries

Two parameters control transactions:

1. **`transactional_ddl`** (per-dialect): `PostgresqlImpl.transactional_ddl = True` (`ddl/postgresql.py:84`). The default `DefaultImpl.transactional_ddl = False` (`ddl/impl.py:106`).

2. **`transaction_per_migration`** (configure() option, default `False`): When `False` and `transactional_ddl=True`, one transaction wraps the entire `run_migrations()` call. When `True`, each migration step gets its own transaction (`runtime/migration.py:145-147`, `372-470`).

The logic in `begin_transaction()` (`runtime/migration.py:372-470`):

```python
if self.impl.transactional_ddl:
  transaction_now = _per_migration == self._transaction_per_migration
else:
  transaction_now = _per_migration is True
```

`runtime/migration.py:419-422`

For non-transactional DDL dialects, even with `transaction_per_migration=True`, each migration still gets its own (non-DDL) transaction boundary.

### Lock strategy

**Alembic has no built-in advisory locking.** A search across all `alembic/` source files finds zero references to `pg_advisory_lock`, `advisory`, or any explicit mutex. Concurrent migration protection is entirely delegated to the user's `env.py`. The Alembic docs recommend wrapping migrations in a `SELECT pg_try_advisory_lock(...)` call inside `env.py` if concurrency is a concern — but that code lives outside Alembic itself.

**Confidence: high** — exhaustive grep confirmed zero advisory lock references in `alembic/` source.

This is a significant gap relative to Djogi's design, which uses `pg_advisory_lock(x'DJOGMIGR'::bigint)` baked in.

### Batch operations for SQLite

`batch_alter_table()` works via `ApplyBatchImpl` (`operations/batch.py:212-481`). The copy-modify-swap pattern:

1. Build a new table definition (named `_alembic_tmp_<tablename>`, max 50 chars: `batch.py:244`)
2. `CREATE TABLE _alembic_tmp_<tablename> (...)` (`batch.py:447`)
3. `INSERT INTO _alembic_tmp_<tablename> SELECT... FROM <tablename>` (`batch.py:450-467`)
4. `DROP TABLE <tablename>` (`batch.py:468`)
5. `ALTER TABLE _alembic_tmp_<tablename> RENAME TO <tablename>` (`batch.py:473-475`)
6. Recreate indexes (`batch.py:478-479`)

On error after step 3: `DROP TABLE _alembic_tmp_<tablename>` rolls back (`batch.py:469-470`).

The `recreate` parameter controls when this path triggers: `"auto"` (dialect decides), `"always"`, `"never"` (`batch.py:72-76`). For SQLite, `requires_recreate_in_batch()` returns `True` when operations other than `add_column` are present.

**Relevance to Djogi:** This is conceptually the same as the `pg_repack` / `gh-ost` online migration pattern — create a shadow table, copy data, swap names. Djogi users doing online-safe column type changes on Postgres could follow this pattern manually. Alembic automates it for SQLite only.

### Offline SQL-script mode (`--sql`)

When `as_sql=True` is passed to `EnvironmentContext`, `MigrationContext` wraps the connection in a `MockConnection` that dumps SQL to `output_buffer` (default: `sys.stdout`) instead of executing it (`runtime/migration.py:151-156`, `669-675`).

`command.upgrade(sql=True)` and `command.downgrade(sql=True)` both flow through this path (`command.py:462-490`, `502-537`). `command.stamp(sql=True)` also works (`command.py:752-796`).

For `--sql` mode, upgrade requires a range `<from>:<to>` is optional for upgrade but required for downgrade (`command.py:473-476`, `519-524`).

The version table `CREATE TABLE` is emitted inline in the SQL output when the database starts from `base` (`runtime/migration.py:617-620`).

**Relevance to Djogi:** Djogi's `build.rs` model generates SQL at build time without a live connection — structurally equivalent to Alembic's `--sql` mode, but Alembic still needs a dialect object to compile SQL. Djogi skips this by generating SQL directly in Rust templates.

---

## Recovery

### Stamp: `command.py:stamp`

`command.stamp()` (`command.py:732-796`) writes revision identifier(s) directly to `alembic_version` without running any migration code. `MigrationContext.stamp()` (`runtime/migration.py:558-572`) calls `script_directory._stamp_revs()` to compute the set of `StampStep` objects, then passes each to `HeadMaintainer.update_to_step()`.

`stamp --purge` deletes all rows first (`command.py:759`, `runtime/migration.py:598-601`). This is the "baseline" workflow: apply `stamp head` after manually bringing the database to a known state.

### Repair semantics

**Alembic has no `repair` command.** `command.py` lists: `list_templates`, `init`, `revision`, `check`, `merge`, `upgrade`, `downgrade`, `show`, `history`, `heads`, `branches`, `current`, `stamp`, `edit`, `ensure_version`. There is no `repair`, `fix`, or `baseline` verb.

The repair workflow is manual:
1. Identify the problem (partially-applied migration, wrong head in `alembic_version`).
2. Manually fix the database schema if needed.
3. Use `alembic stamp <revision>` to declare the current state.

**Confidence: high** — exhaustive function listing from `command.py`.

### Recovery from failed migration

Alembic provides no automatic rollback of partially-applied DDL. Because `transaction_per_migration=False` by default, a failure mid-migration leaves the database in a partially-migrated state with no record of what succeeded — the `alembic_version` row is only updated *after* `step.migration_fn(**kw)` returns successfully (`runtime/migration.py:626-633`). If the function raises, the version table is not updated, but DDL already emitted (for non-transactional DDL) is not rolled back.

For transactional DDL (PostgreSQL), the outer transaction (or per-migration transaction if `transaction_per_migration=True`) will roll back the DDL along with the version update. The database returns to its pre-migration state.

### Partial apply on non-transactional DDL

There is **no partial-apply tracking** in the `alembic_version` table. The table has only `version_num`. If a migration fails mid-way on a non-transactional DDL database (e.g., MySQL's `ALTER TABLE`), the version row is not written, but some DDL may have been committed. Recovery requires the user to:
1. Inspect what succeeded.
2. Manually revert or complete the DDL.
3. `stamp` to the appropriate revision.

**Confidence: high** — verified by inspecting `alembic_version` schema (one column only) and the execution path in `runtime/migration.py:614-633`.

---

## Diff and generation (autogenerate)

### The autogenerate pipeline

Entry point: `autogenerate/api.py:50-173` (`compare_metadata()` → `produce_migrations()` → `AutogenContext` → `compare._populate_migration_script()`).

`_populate_migration_script()` (`autogenerate/compare/__init__.py:33-40`) calls `_produce_net_changes()`, which dispatches via `autogen_context.comparators` (a `PriorityDispatcher`) at the `"autogenerate"` level. This dispatches to `_produce_net_changes` in `compare/schema.py:25-55`, which fans out to the `"schema"` level comparator `_autogen_for_tables` in `compare/tables.py`.

The `comparators` is a plugin-based `PriorityDispatcher` registered in `compare/__init__.py:23`, `53-62`. Each `setup(plugin)` call in a compare submodule registers its comparators at different priorities and qualifier (dialect name) scopes.

### How `compare_metadata` works

1. `_autogen_for_tables()` (`compare/tables.py:36-83`) gets `inspector.get_table_names()` for each schema, excludes the `alembic_version` table (`tables.py:52-55`, `73`), applies `run_name_filters()`, then calls `_compare_tables()`.

2. `_compare_tables()` (`compare/tables.py:86-232`): Sets = metadata tables vs. connection tables. Tables in metadata but not connection → `CreateTableOp`. Tables in connection but not metadata → `DropTableOp`. For existing tables, calls `_compare_columns()` then dispatches the `"table"` comparator (which triggers constraints, indexes, FK comparators).

3. `_compare_columns()` (`compare/tables.py:235-307`): For each column in metadata not in connection → `AddColumnOp`. For each column in both → `AlterColumnOp` (dispatches `"column"` comparator for type, nullable, server_default). For each column in connection not in metadata → `DropColumnOp`.

4. `_compare_indexes_and_uniques()` (`compare/constraints.py:53-441`): Reflects `get_unique_constraints()` and `get_indexes()`, converts to `_constraint_sig` objects for consistent comparison, then computes added/removed/changed by name and by column-set signature.

5. `_compare_foreign_keys()` (`compare/constraints.py:626-714`): Reflects `get_foreign_keys()`, computes added/removed FKs by column+referent signature.

6. Type comparison is a two-stage dispatch (`compare/types.py`): `_user_compare_type` (user-provided callable, `FIRST` priority) then `_dialect_impl_compare_type` (`LAST` priority). Type comparison is **on by default** as of Alembic 1.12.0 (`runtime/environment.py:580-582`).

7. Server default comparison is **off by default** (`runtime/environment.py:590-597`).

After `_produce_net_changes()`, the upgrade ops are reversed into downgrade ops via `upgrade_ops.reverse_into(downgrade_ops)` (`compare/__init__.py:40`).

### What diffs it detects

Confirmed in source:
- Tables: added, removed (`compare/tables.py:120-175`)
- Columns: added, removed, type changed (if `compare_type=True`), nullable changed, server default changed (if `compare_server_default=True`) (`compare/tables.py:261-307`, `compare/types.py`, `compare/server_defaults.py`)
- Indexes: added, removed, changed (uniqueness, column set) (`compare/constraints.py:53-441`)
- Unique constraints: added, removed, changed (`compare/constraints.py:53-441`)
- Foreign key constraints: added, removed (`compare/constraints.py:626-714`)
- Table/column comments: added, removed (`compare/comments.py`)
- Computed columns (server_default with `Computed`): detected with a warning if changed, since they cannot be altered (`compare/server_defaults.py:61-118`)

### What it misses by default

Confirmed by source inspection:
- **Check constraints:** `_add_check_constraint` raises `NotImplementedError()` (`render.py:441-442`). Check constraints are **not** auto-detected or rendered. The `"check_constraints"` key is present in `_InspectorConv`'s reflection cache keys (`compare/util.py:30, 38`) but no comparator dispatches on it.
- **Sequences:** Not compared in any `compare/` submodule.
- **Column renames:** No rename heuristic exists anywhere in `alembic/autogenerate/compare/`. A rename appears as `drop_column` + `add_column`. This is intentional — autogenerate does not detect renames. (`RenameTableOp` exists in `ops.py:1451-1485` as an explicit user-invoked operation, not an autogenerate output.)
- **Table renames:** Same — appears as drop + create.

**Confidence: high** — verified by grep and source read for each claim.

### Rename handling

Autogenerate does **not** detect renames, for either tables or columns. The compare pipeline has no heuristic rename detection. A renamed column produces `DropColumnOp` + `AddColumnOp`.

`RenameTableOp` (`ops.py:1451-1485`) is available as an explicit operation for hand-written migrations only, not as autogenerate output.

**Confidence: high** — zero rename detection logic found in `autogenerate/compare/`.

### `include_object` / `include_name` / `compare_type` / `compare_server_default` hooks

All four are parameters to `EnvironmentContext.configure()` (`runtime/environment.py:420-446`):

- **`include_name`** (`runtime/environment.py:428`, `633-668`): Callable `(name, type_, parent_names) -> bool`. Filters by name before reflection. `type_` can be `"schema"`, `"table"`, `"column"`, `"index"`, `"unique_constraint"`, `"foreign_key_constraint"`.
- **`include_object`** (`runtime/environment.py:429`, `675-729`): Callable `(object, name, type_, reflected, compare_to) -> bool`. Filters SQLAlchemy schema objects after reflection. Receives the actual `Table`, `Column`, `Index`, etc.
- **`compare_type`** (`runtime/environment.py:434`, `571-587`): `True` (default since 1.12), `False`, or callable `(context, inspected_col, metadata_col, inspected_type, metadata_type) -> bool|None`. Stored at `MigrationContext._user_compare_type` (`runtime/migration.py:183`).
- **`compare_server_default`** (`runtime/environment.py:435`, `590-631`): `False` (default), `True`, or callable. Stored at `MigrationContext._user_compare_server_default` (`runtime/migration.py:184`).

`AutogenContext` stores the name/object filters as lists (`api.py:387-397`) and exposes `run_name_filters()` (`api.py:423-458`) and `run_object_filters()` (`api.py:460-483`).

### Render: how `operations/ops.py` generates Python migration code

`render.py` maintains a `renderers` dispatcher. Each operation class is decorated with `@renderers.dispatch_for(OpClass)` to register its renderer. Examples:
- `CreateUniqueConstraintOp` → `_add_unique_constraint` → calls `_uq_constraint()` which renders `op.create_unique_constraint(name, table, [cols])` (`render.py:381-385`, `657-697`)
- `CreateForeignKeyOp` → `_add_fk_constraint` → renders `op.create_foreign_key(name, src, ref, local_cols, remote_cols,...)` (`render.py:388-432`)
- `AlterColumnOp` → rendered with `modify_type`, `modify_nullable`, `modify_server_default`, `existing_*` kwargs

Composite unique constraints render their column list as a Python list: `repr([_ident(col.name) for col in constraint.columns])` (`render.py:683`). Composite FK constraints also render both column lists (`render.py:397-402`).

---

## Schema metadata (via SQLAlchemy)

Alembic's autogenerate consumes SQLAlchemy metadata objects:

- **`Table`** — the top-level object. `AutogenContext.sorted_tables` aggregates `MetaData.sorted_tables` across potentially multiple `MetaData` instances (`api.py:486-499`).
- **`Column`** — carries type, nullable, server_default. Used in `_compare_columns()`.
- **`Index`** — compared by column set and uniqueness in `_compare_indexes_and_uniques()`.
- **`UniqueConstraint`** — distinguished from `Index` for databases that represent them separately. Rendered as `sa.UniqueConstraint(*cols)` inline in `CreateTableOp` or as `op.create_unique_constraint(...)` as an alter.
- **`ForeignKeyConstraint`** — compared by referent table + column sets in `_compare_foreign_keys()`.
- **`PrimaryKeyConstraint`** — reflected but `CreatePrimaryKeyOp` autogenerate rendering raises `NotImplementedError()` (`render.py:436-437`).
- **`CheckConstraint`** — `CreateCheckConstraintOp` rendering raises `NotImplementedError()` (`render.py:440-442`).

### Naming conventions via `MetaData(naming_convention=...)`

SQLAlchemy's `conv()` wrapper is used in `_InspectorConv` to mark reflected constraint names so Alembic knows they were assigned by the naming convention, not hand-coded. This prevents false "name changed" diffs (`compare/util.py:87-102`). The batch module passes `naming_convention` to the temp-table `MetaData` (`batch.py:117-120`), ensuring constraints created during the copy retain correct generated names.

Without naming conventions, anonymous constraints generate different names per run, causing spurious diffs. This is the primary motivation for `MetaData(naming_convention={"ix": "ix_%(table_name)s_%(column_0_label)s",...})`.

### Composite constraints rendering

Composite unique constraints: `repr([_ident(col.name) for col in constraint.columns])` — renders as a list of column names passed to `op.create_unique_constraint()` or `sa.UniqueConstraint()` (`render.py:683, 691`).

Composite foreign key constraints: both `local_cols` and `remote_cols` are rendered as lists (`render.py:397-402`).

---

## Online-safe / staged migration guidance

### Does Alembic document online-safe patterns?

Alembic documents `autocommit_block()` for Postgres DDL that must run outside transactions (e.g., `CREATE INDEX CONCURRENTLY`):

```python
def upgrade():
  with op.get_context().autocommit_block():
    op.execute("ALTER TYPE mood ADD VALUE 'soso'")
```

`runtime/migration.py:279-370`

The `autocommit_block()` unconditionally commits the preceding transaction (`runtime/migration.py:310-319`). The docstring warns that the migration preceding the block is committed before the operation completes, and recommends `transaction_per_migration=True` when using autocommit blocks.

### Batch migrations for SQLite

Mechanism: `ApplyBatchImpl._create()` (`operations/batch.py:442-481`): create temp table → bulk copy → drop original → rename temp. This is the copy-modify-swap pattern. Triggered when `recreate="auto"` (default) and the dialect's `requires_recreate_in_batch()` returns `True` (SQLite only).

For `--sql` mode with batch, `copy_from` must be provided as a `Table` object because reflection needs a live connection (`batch.py:127-138`).

### `executionoptions` for non-transactional DDL

`MigrationContext.execute()` passes `execution_options` through to `impl._exec()` (`runtime/migration.py:654-667`, `ddl/impl.py:216-246`). This allows per-statement options. The `autocommit_block()` context manager uses `execution_options(isolation_level="AUTOCOMMIT")` on the connection itself (`runtime/migration.py:344-346`).

### Data migration companions

Not a built-in Alembic concept. Users write separate migration scripts that combine DDL + DML. No "expand/contract" or "shadow table" automation exists in Alembic.

---

## Failure modes

### Failure with `transaction_per_migration=False` (default) on PostgreSQL

Because `PostgresqlImpl.transactional_ddl = True`, the entire `run_migrations()` call is wrapped in one transaction (when `transaction_per_migration=False`). If any migration step raises, the transaction rolls back, `alembic_version` is not updated, and the database returns to its pre-migration state. Recovery: fix the migration, re-run.

### Failure with `transaction_per_migration=True` on PostgreSQL

Each migration step commits before the next begins. A failure in step N leaves migrations 0..N-1 applied and committed. The version table reflects steps 0..N-1. Recovery: fix step N's migration script, re-run (which skips already-applied revisions).

### Failure on non-transactional DDL (e.g., MySQL)

DDL committed before the error is permanent. The `alembic_version` row is not updated (it's only written after `migration_fn()` succeeds: `runtime/migration.py:626-633`). The database is in a partially-migrated state with no automated way to determine which statements within the migration succeeded. Recovery:
1. Manually inspect the schema.
2. Manually complete or revert DDL.
3. `alembic stamp <rev>` to record the correct state.

### How partial apply is recorded

**It is not recorded.** The `alembic_version` table stores only the completed `version_num` per active head. There is no `in_progress`, `checksum`, `failed`, or `partial` column. If a migration fails, no row is written.

**Confidence: high** — verified by complete schema reading (`ddl/impl.py:170-183`) and execution flow reading (`runtime/migration.py:614-633`).

---

## env.py contract

### What `env.py` is

`env.py` is a user-editable Python script loaded by `ScriptDirectory.run_env()` (`script/base.py:536-546`) via `util.load_python_file(self.dir, "env.py")`. It is loaded fresh on every `alembic` CLI invocation. It is the sole integration point between Alembic and the user's application.

### `run_migrations_online` vs `run_migrations_offline`

The generic template (`templates/generic/env.py:29-78`) defines both functions:

- **`run_migrations_offline()`**: Calls `context.configure(url=...,...)` with just a URL, then `context.begin_transaction()` + `context.run_migrations()`. Produces SQL output without a live connection.
- **`run_migrations_online()`**: Creates an `Engine`, gets a `Connection`, calls `context.configure(connection=connection, target_metadata=target_metadata)`, then `context.begin_transaction()` + `context.run_migrations()`.

`context.is_offline_mode()` selects the branch (`templates/generic/env.py:75-78`).

### Why this extensibility matters

Because `env.py` is arbitrary Python, users can:
- Pull connection strings from environment variables, AWS Secrets Manager, Vault, etc.
- Register `include_object`, `include_name`, `compare_type`, `compare_server_default` hooks.
- Acquire advisory locks before `run_migrations()`.
- Use `process_revision_directives` to post-process autogenerated revisions.
- Run multiple databases by calling `context.configure()` + `context.run_migrations()` multiple times (multidb template).
- Override `transaction_per_migration`, `transactional_ddl`, and all other options per-invocation.

Flyway and Liquibase provide no equivalent hook — their configuration is entirely declarative (XML, YAML, properties files). Alembic's `env.py` is uniquely powerful.

---

## Branching and merging

### How branches work

A branch is created when `alembic revision --head <non-head-revision> --splice` is used, or when two independent revision chains exist with different root revisions. Each branch has its own `head`. Multiple active heads are tracked as multiple rows in `alembic_version`.

`alembic branches` command (`command.py:662-688`) iterates all revisions and prints those where `sc.is_branch_point` is true (i.e., `len(self.nextrev) > 1`, `revision.py:1679-1688`).

### How merges work

`command.merge()` (`command.py:382-446`) calls `script.generate_revision(rev_id, message, head=revisions)` where `revisions` is a tuple of two-or-more revision identifiers. The resulting merge revision file has `down_revision = ('abc123', 'def456')` — a tuple. This makes it a merge point (`is_merge_point` = True, `revision.py:1699-1702`).

When the merge migration is applied, `HeadMaintainer.should_merge_branches()` detects the tuple `down_revision` with multiple heads present (`runtime/migration.py:1170-1179`), and `merge_branch_idents()` computes which rows to delete and which to update (`runtime/migration.py:1090-1113`).

### Why this is unique

Alembic is the only tool in common use that models migrations as a true DAG with explicit branch and merge semantics in the history table. Flyway, Liquibase, and Django all use linear (or at most weakly-ordered) history models. Django's `RunPython` squash and Flyway's `repeatable` migrations are workarounds for the same underlying limitation.

### Whether Djogi should support this

**Not for 0.1.0.** Djogi's `NNNN_name_up.sql` / `NNNN_name_down.sql` design implies linear sequential numbering. Supporting DAG branches would require:
- Non-sequential identifiers (like Alembic's random hex)
- Multiple active head tracking in the ledger
- A merge concept in the runner

**Defer to 0.2+.** The use case is primarily monorepos with parallel feature branches that each add migrations. For a 0.1.0 Postgres-only ORM, linear NNNN_ numbering with out-of-order detection is sufficient. If branches become a real pain point, adopt Alembic's approach: random IDs + multi-row head table.

---

## Lessons for Djogi

### Adopt

- **Separate upgrade and downgrade as first-class functions** (confirmed in `RevisionStep`: `migration_fn = revision.module.upgrade` or `.downgrade`, `runtime/migration.py:1009-1012`). Djogi already does this with `_up.sql`/`_down.sql` pairs. This is correct.

- **`stamp` / baseline as a distinct first-class command** (`command.py:732-796`, `runtime/migration.py:558-572`). Djogi's spec includes this. Alembic's implementation is clean: compute `StampStep` objects from the revision graph, then run them through `HeadMaintainer` exactly like real migrations, except `StampStep.stamp_revision()` is a no-op.

- **`compare_type=True` as the default** (`runtime/environment.py:580-582`). Djogi's differ should compare column types. Alembic's experience: enabling this by default in 1.12.0 was the right call.

- **Exclude the ledger table from autogenerate** (`compare/tables.py:52-55`, `73`): `tables.difference([autogen_context.migration_context.version_table])`. Djogi's differ must do the same for its own ledger table.

- **Dispatch-based comparator architecture** (`compare/__init__.py:23`, `53-62`): Plugin-based, priority-ordered, dialect-qualified dispatch allows extending the diff pipeline without modifying core code. Djogi doesn't need plugins for 0.1.0, but the architecture is worth emulating — a dispatch table per comparison type (tables, columns, constraints, types) is cleaner than one monolithic diff function.

- **`include_object` / `include_name` hooks** (`runtime/environment.py:428-429`): Letting users filter what gets diffed is essential for mixed-owner schemas. Djogi should expose equivalent hooks in its config (e.g., `[djogi] include_tables =...` or a callback).

- **Offline SQL generation without connection** (`runtime/migration.py:151-156`, `669-675`): The `--sql` / `as_sql=True` mode is directly analogous to Djogi's `build.rs` model. Djogi goes further by generating SQL at compile time, not runtime. This is strictly better — no dialect object needed at generation time.

- **`autocommit_block()` pattern for non-transactional DDL** (`runtime/migration.py:279-370`): For Postgres DDL that must run outside transactions (`CREATE INDEX CONCURRENTLY`, `ALTER TYPE... ADD VALUE`), expose an explicit escape hatch. Djogi's non-transactional DDL auto-split is related — but Alembic's `autocommit_block()` is more surgical.

### Reject

- **No advisory lock** — Alembic has none. Djogi should not follow this omission. The `pg_advisory_lock(x'DJOGMIGR'::bigint)` baked into the runner is the right decision. Concurrent migration attempts on production databases are a real failure mode.

- **No partial-apply tracking in the ledger** — Alembic's single `version_num` column gives no diagnostic information after a failure on non-transactional DDL. Djogi's ledger with `execution_mode`, `partial_apply` state, checksum, and `applied_at` timestamp is strictly better.

- **No checksum enforcement** — Alembic never verifies that applied migration files match their on-disk versions. Djogi's checksums detect accidental or malicious modification of committed migrations. Adopt checksums; reject Alembic's laxity here.

- **Random hex revision IDs** — Djogi's `NNNN_name` sequential format is more readable for debugging, easier to sort, and gives a human-visible ordering guarantee. The tradeoff is that parallel feature branches need a squash/rebase discipline. For Postgres-only, single-codebase usage, `NNNN` is preferable.

- **No built-in repair command** — Alembic's answer is "use `stamp`." Djogi should make repair a first-class verb that validates checksums, detects out-of-order applied migrations, and offers structured remediation.

- **Python `env.py` as the integration point** — Djogi is Rust. The `env.py` model is powerful but creates a runtime Python dependency and a seam that can silently diverge from the tool's expectations. Djogi's approach of configuration in `Cargo.toml` / `djogi.toml` + a compiled runner is cleaner.

### Defer

- **Branching and merging** — valuable for monorepos with parallel feature branches, but out of scope for 0.1.0. Revisit when users report pain from sequential numbering conflicts on concurrent branches.

- **Multi-`MetaData` autogenerate** (`api.py:487-524`) — Alembic supports aggregating multiple `MetaData` objects for multi-database schemas. Djogi's 0.1.0 is single-database; defer.

- **`process_revision_directives` hook** (`runtime/environment.py:431-433`) — allows post-processing the generated migration AST (e.g., removing no-op migrations, rewriting certain ops). Useful for advanced workflows; defer to a later Djogi plugin system.

- **Post-write hooks** (black formatting, custom scripts after script generation; `script/write_hooks.py`) — nice-to-have; defer.

- **`compare_server_default`** — Alembic defaults this to `False` because server default comparison is dialect-dependent and fragile (some backends execute defaults on the DB side to compare). For Postgres 18 specifically, it can be made accurate; defer.

### Surprises

1. **`compare_type=True` is the default since 1.12.0** (`runtime/environment.py:580-582`). This contradicts older documentation and articles. Djogi's differ should also default to type comparison on.

2. **`_add_check_constraint` raises `NotImplementedError()`** (`render.py:440-442`). Check constraints are reflected (`compare/util.py:30`) but autogenerate cannot compare or render them. This is a known Alembic limitation. Djogi's differ should handle `CHECK` constraints, especially given Postgres 18's improved check constraint support.

3. **The PK on `alembic_version` is opt-out** (`runtime/environment.py:540-544`, `ddl/impl.py:176-181`). Older Alembic deployments may lack the PK (`version_table_pk=False`). This means the table was historically just a bare column with no constraint — concurrent inserts could produce duplicate rows. The named constraint `alembic_version_pkc` was added later. Djogi should start with a proper PK from day one.

4. **No lock → race condition on concurrent deploys.** With no advisory lock and `transaction_per_migration=False`, two simultaneous `alembic upgrade head` runs on PostgreSQL will both read the same current head, both begin the same migration, and the second will deadlock or produce constraint violations on `alembic_version`. The PK on `version_num` provides some protection (duplicate insert fails), but this is not a clean solution.

5. **`batch_alter_table` is the copy-modify-swap pattern.** The temp table name is `_alembic_tmp_<tablename>` (max 50 chars, `batch.py:244`). If a previous batch operation failed, the temp table may still exist. Alembic has no cleanup for orphaned temp tables. Djogi's non-transactional DDL handling should plan for cleanup.

6. **`alembic check` as CI gate** (`command.py:323-378`): Raises `AutogenerateDiffsDetected` if there are unapplied model changes. This is directly relevant to Djogi's `build.rs` model: a CI step that runs `cargo build` and checks that the generated migration is empty (or committed) provides the same guarantee.

---

## Confidence

| Section | Confidence | Notes |
|---|---|---|
| `alembic_version` DDL (exact) | high | Read `ddl/impl.py:151-183` |
| Version table role (no history) | high | Read `runtime/migration.py:499-542` |
| Multiple heads (multi-row) | high | Read `HeadMaintainer` in full |
| Revision ID as UUID4 trailing hex | high | Read `langhelpers.py:231-232` |
| Transaction boundaries | high | Read `runtime/migration.py:372-470`, `ddl/postgresql.py:84` |
| No advisory lock | high | Exhaustive grep: zero results |
| No repair command | high | Listed all functions in `command.py` |
| No partial-apply tracking | high | Schema read + execution path read |
| Autogenerate: what it detects | high | Read all `compare/` submodules |
| Check constraint not auto-detected | high | `render.py:441-442` raises `NotImplementedError` |
| Rename not auto-detected | high | Zero rename detection in `compare/` |
| batch_alter_table copy-modify-swap | high | Read `batch.py:442-481` |
| Offline `--sql` mode | high | Read `runtime/migration.py:151-156, 617-620` |
| Branch/merge mechanics | high | Read `revision.py:1679-1702`, `runtime/migration.py:1090-1179` |
| `include_object`/`include_name` hooks | high | Read `runtime/environment.py:428-729` |
| `compare_type` default changed to True | high | Read `runtime/environment.py:580-582` |
| Online-safe patterns (docs) | medium | Read `autocommit_block()` in source; broader cookbook docs not read (docs/build only) |
| `compare_server_default` accuracy on Postgres | medium | Source shows the option exists and a callable is accepted; did not trace Postgres-specific comparison logic |

### Parts not verified

- `docs/` directory was not read (only `build/` exists in the clone; source docs are `.rst` files not present or not inspected).
- The async template variants (`templates/async/`) were not read.
- `alembic/testing/` and `tests/` were not read.
- `alembic/ddl/sqlite.py` `requires_recreate_in_batch()` logic was not read in full.

---

## Open questions for synthesis

1. **Lock gap across tools:** Does any surveyed tool besides Flyway use an advisory lock for concurrency control? If not, Djogi's `pg_advisory_lock` approach is a differentiator worth documenting explicitly.

2. **Check constraint comparison:** Alembic cannot autogenerate check constraint diffs. Does Django or Liquibase do better? If Djogi's differ handles `CHECK`, it closes a real gap vs. all surveyed tools.

3. **Ledger minimalism vs. richness:** Alembic's one-column ledger is the extreme minimal. Django, Flyway, and Liquibase all have richer ledgers. Does ledger richness correlate with better recovery UX? Cross-project comparison needed.

4. **Sequential vs. random IDs:** Alembic (random) vs. Flyway/Django (sequential/timestamp). What failure modes does each approach have at scale, especially in CI/CD pipelines where migrations are generated on parallel branches?

5. **Stamp / fake migration:** Both Alembic (`stamp`) and Flyway (`baseline`) have this. Does every surveyed tool have it? It should be a Djogi 0.1.0 requirement.

6. **`process_revision_directives` equivalent in Djogi:** Alembic allows post-processing the autogenerated migration AST. Djogi's `build.rs` model generates SQL — should there be a hook to post-process or reject generated SQL before it's committed? This connects to the "no heuristic rename" design decision.

7. **Copy-modify-swap for online Postgres migrations:** Alembic's `batch_alter_table` is SQLite-specific, but `pg_repack` uses the same pattern for Postgres. Should Djogi document or partially automate `pg_repack`-style migrations as a named migration mode? Worth cross-referencing with the `pg_repack` / `gh-ost` research note.
