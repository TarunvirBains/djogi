# Django

## Metadata
- Clone path: `/home/tarunvir/projects/django-reference/`
- Commit SHA inspected: `69d86004f7b3c9ed223c18998c2b799d1670474f`
- Primary language: Python
- Migration-relevant modules:
 - `django/db/migrations/autodetector.py`
 - `django/db/migrations/executor.py`
 - `django/db/migrations/graph.py`
 - `django/db/migrations/loader.py`
 - `django/db/migrations/migration.py`
 - `django/db/migrations/optimizer.py`
 - `django/db/migrations/questioner.py`
 - `django/db/migrations/recorder.py`
 - `django/db/migrations/serializer.py`
 - `django/db/migrations/state.py`
 - `django/db/migrations/writer.py`
 - `django/db/migrations/operations/base.py`
 - `django/db/migrations/operations/fields.py`
 - `django/db/migrations/operations/models.py`
 - `django/db/migrations/operations/special.py`
 - `django/contrib/postgres/operations.py` (Postgres-specific: `AddIndexConcurrently`, `RemoveIndexConcurrently`, etc.)
 - `django/db/backends/base/schema.py` (schema editor — atomic transaction boundary logic)
 - `django/core/management/commands/migrate.py` (`--fake`, `--fake-initial` CLI)
 - `django/core/management/commands/squashmigrations.py`
- Approximate LOC of migration-relevant code:
 - `django/db/migrations/` (all files): **8,030 lines**
 - `django/contrib/postgres/operations.py`: 352 lines
 - `django/db/backends/base/schema.py` (migration-relevant portion): ~100 lines
 - Total migration-relevant: approximately **8,500 lines**

---

## Architecture
- Module layout of `django/db/migrations/`:
 - `recorder.py` — `MigrationRecorder`; owns `django_migrations` table DDL and all read/write against it
 - `executor.py` — `MigrationExecutor`; orchestrates plan computation and actual apply/unapply
 - `loader.py` — `MigrationLoader`; scans disk, cross-references with DB, builds the graph, handles squash replacements
 - `graph.py` — `MigrationGraph` + `Node` + `DummyNode`; directed acyclic graph with iterative DFS traversal
 - `state.py` — `ProjectState`, `ModelState`, `StateApps`; in-memory schema state machine
 - `autodetector.py` — `MigrationAutodetector`; computes diff of two `ProjectState` objects into ordered migration operations
 - `migration.py` — `Migration` base class; owns `apply()`, `unapply()`, `mutate_state()`
 - `optimizer.py` — `MigrationOptimizer`; merges redundant operations (e.g., `CreateModel` + `AddField` → single `CreateModel`)
 - `questioner.py` — `MigrationQuestioner`, `InteractiveMigrationQuestioner`, `NonInteractiveMigrationQuestioner`; interactive yes/no prompts for renames, nullable defaults, etc.
 - `serializer.py` — serializes `Operation` instances to Python source code for migration files
 - `writer.py` — writes the final `.py` migration file to disk
 - `operations/base.py` — `Operation` base class and `OperationCategory` enum
 - `operations/fields.py` — `AddField`, `RemoveField`, `AlterField`, `RenameField`
 - `operations/models.py` — `CreateModel`, `DeleteModel`, `RenameModel`, `AlterUniqueTogether`, `AddIndex`, `AddConstraint`, `RemoveConstraint`, `AlterConstraint`, `RenameIndex`, etc.
 - `operations/special.py` — `RunSQL`, `RunPython`, `SeparateDatabaseAndState`

---

## State model (source-of-truth)

**How models → migration files → applied-state is tracked:**

Django maintains two distinct planes of state:

1. **On-disk migration files** — Python modules containing a `Migration` subclass with `operations`, `dependencies`, `replaces`, and an optional `atomic` flag. The `MigrationLoader` scans every installed app's `migrations/` directory, imports each file, and instantiates the `Migration` class. (`django/db/migrations/loader.py:74-140`)

2. **Applied-state in the database** — A row in `django_migrations` for every migration that has been applied. `MigrationRecorder.applied_migrations()` returns a `dict[(app, name) -> Migration instance]`. (`django/db/migrations/recorder.py:84-97`)

3. **In-memory `ProjectState`** — A `ProjectState` instance is built by replaying all applied migrations in topological order: each `operation.state_forwards(app_label, state)` call mutates the state in sequence. The final `ProjectState` represents the current database schema as Django understands it. (`django/db/migrations/state.py:95-110`, `django/db/migrations/migration.py:80-92`)

**Role of `django_migrations` table:**

The single source of truth for which migrations have been applied. Presence of a row means applied; absence means not applied. There is no "pending" or "failed" state recorded: a row is only inserted on successful completion, and removed on rollback. (`django/db/migrations/recorder.py:9-20`, `recorder.py:99-107`)

**Role of `MigrationGraph` and `ProjectState` in memory:**

- `MigrationGraph` holds all migration nodes and edges (dependencies). It provides `forwards_plan(target)` (iterative DFS producing a topologically sorted list) and `backwards_plan(target)` (reverse DFS). (`django/db/migrations/graph.py:201-238`)
- `ProjectState` is the in-memory schema. It is cloned before each operation so that pre- and post-operation states can be passed to `operation.database_forwards(app_label, schema_editor, old_state, new_state)`. (`django/db/migrations/migration.py:117-134`)
- There is no persistent `schema_snapshot.json` equivalent in Django. The schema state is always reconstructed at runtime by replaying migrations.

**Separation of applied-state from execution history:**

Django does not record execution history, timing, checksums, or execution mode. The `django_migrations` table records only that a migration is applied, not when, how long it took, or whether it ran transactionally. (`django/db/migrations/recorder.py:32-46`)

---

## Ledger / history table

**Exact DDL (reconstructed from model definition in source):**

Django does not store raw DDL; the table is created via the ORM's `schema_editor.create_model()`. The authoritative model is:

```python
# django/db/migrations/recorder.py:32-46
class Migration(models.Model):
  app = models.CharField(max_length=255)
  name = models.CharField(max_length=255)
  applied = models.DateTimeField(default=now)

  class Meta:
    apps = Apps()
    app_label = "migrations"
    db_table = "django_migrations"
```

Reconstructed SQL for PostgreSQL:

```sql
CREATE TABLE "django_migrations" (
  "id"   serial PRIMARY KEY,
  "app"   varchar(255) NOT NULL,
  "name"  varchar(255) NOT NULL,
  "applied" timestamp with time zone NOT NULL
);
```

There is no explicit unique constraint declared in the model. The ORM does not add one automatically unless `unique_together` or `UniqueConstraint` is specified, neither of which appears in this `Meta`. Django relies on the combination of (app, name) being logically unique by application-level discipline (the record is only inserted once on apply, and deleted on unapply).

**Column purposes:**
- `id` — surrogate primary key (auto-increment)
- `app` — the Django app label (e.g., `"auth"`, `"myapp"`)
- `name` — the migration name without `.py` (e.g., `"0001_initial"`)
- `applied` — timestamp of when the migration was recorded as applied

**Primary key / unique constraints:**

Implicit surrogate `id` primary key only. No composite unique constraint on `(app, name)` is declared in source. (`django/db/migrations/recorder.py:32-46`)

**Indexes:**

None declared beyond the implicit primary key index.

---

## Execution

**Lock strategy:**

Django has **no advisory lock or distributed lock** on migration execution. The schema editor opens a database transaction (if `atomic=True`), which implicitly acquires table-level or row-level locks as DDL executes, but there is no explicit `pg_advisory_lock` or equivalent. Multiple concurrent `manage.py migrate` runs can race and corrupt the applied-state table. (`django/db/migrations/executor.py`, `django/db/backends/base/schema.py` — no lock call found in either file)

**Transaction boundaries:**

- Default: the `SchemaEditor` opens a `BEGIN`/`COMMIT` block wrapping the entire migration. This is conditional on `connection.features.can_rollback_ddl` (true for PostgreSQL). (`django/db/backends/base/schema.py:151-172`)

 ```python
 # django/db/backends/base/schema.py:156
 self.atomic_migration = self.connection.features.can_rollback_ddl and atomic
 ```

 ```python
 # django/db/backends/base/schema.py:160-164
 def __enter__(self):
   self.deferred_sql = []
   if self.atomic_migration:
     self.atomic = atomic(self.connection.alias)
     self.atomic.__enter__()
 ```

- `atomic = False` opt-out: if `Migration.atomic = False`, the `SchemaEditor` is constructed with `atomic=False`, bypassing the transaction wrapper entirely. (`django/db/migrations/executor.py:254-257`)

 ```python
 # django/db/migrations/executor.py:254-257
 with self.connection.schema_editor(
   atomic=migration.atomic
 ) as schema_editor:
   state = migration.apply(state, schema_editor)
 ```

- Per-operation atomic: inside a non-atomic migration, individual operations can still be wrapped in a transaction if `operation.atomic` is set. The logic in `migration.apply()` handles this:

 ```python
 # django/db/migrations/migration.py:120-133
 atomic_operation = operation.atomic or (
   self.atomic and operation.atomic is not False
 )
 if not schema_editor.atomic_migration and atomic_operation:
   with atomic(schema_editor.connection.alias):
     operation.database_forwards(...)
 else:
   operation.database_forwards(...)
 ```

- Note: `deferred_sql` (indexes, unique constraints) is executed *after* the main migration body, inside `SchemaEditor.__exit__` before the outer transaction commits. (`django/db/backends/base/schema.py:167-172`)

**How non-transactional DDL is handled:**

Django ships `AddIndexConcurrently` and `RemoveIndexConcurrently` in `django.contrib.postgres.operations`. Both set `atomic = False` at the class level and use a `NotInTransactionMixin` that raises `NotSupportedError` if called inside a transaction block:

```python
# django/contrib/postgres/operations.py:123-126
class AddIndexConcurrently(NotInTransactionMixin, AddIndex):
  """Create an index using PostgreSQL's CREATE INDEX CONCURRENTLY syntax."""
  atomic = False
  category = OperationCategory.ADDITION
```

```python
# django/contrib/postgres/operations.py:114-120
class NotInTransactionMixin:
  def _ensure_not_in_transaction(self, schema_editor):
    if schema_editor.connection.in_atomic_block:
      raise NotSupportedError(
        "The %s operation cannot be executed inside a transaction "
        "(set atomic = False on the migration)." % self.__class__.__name__
      )
```

There is no auto-split into segments. The user must manually create a migration with `atomic = False` containing only the `AddIndexConcurrently` operation.

**Concurrency posture:**

No protection against concurrent runners. No global lock, no application-level check for another runner in progress. This is a known limitation; Django's documentation advises deploying migrations serially. (`django/db/migrations/executor.py` — no lock logic present)

---

## Recovery

**Checksum algorithm:**

Django does **not** checksum migration files. There is no hash stored in `django_migrations` and no integrity check comparing disk content against what was applied. A migration file can be edited after application without Django detecting the change. (`django/db/migrations/recorder.py` — no hash/checksum field or computation)

**Repair commands and semantics:**

Django has no built-in "repair" command. The closest approximations are:
- `--fake`: marks a migration as applied without running it (`django/core/management/commands/migrate.py:52-55`)
- `--prune`: deletes rows from `django_migrations` for migrations that no longer exist on disk (`django/core/management/commands/migrate.py:85-90`)
- Squash + manual deletion of old migration rows

**`--fake` and `--fake-initial` (baseline adoption):**

```
# django/core/management/commands/migrate.py:52-64
"--fake": Mark migrations as run without actually running them.
"--fake-initial": Detect if tables already exist and fake-apply initial migrations if so.
```

- `--fake`: the executor calls `record_migration()` directly without calling `migration.apply()`. (`django/db/migrations/executor.py:241-266`, specifically the `if not fake:` guard at line 246)
- `--fake-initial`: triggers `detect_soft_applied()` which inspects the live database for table/column existence. If found, the migration is faked. (`django/db/migrations/executor.py:247-252`, `310-413`)

**Partial-apply handling:**

There is no partial-apply state. A migration is either fully applied (row present) or not applied (row absent). If a migration fails mid-way on a transactional DDL backend (e.g., PostgreSQL), the transaction is rolled back and no row is written. On a non-transactional migration (`atomic = False`), if it fails mid-way, there is no rollback: the already-executed DDL statements persist in the database, and no row is written to `django_migrations`. Django has no mechanism to detect or recover from this partial state. (`django/db/migrations/executor.py:241-266`, `django/db/backends/base/schema.py:167-172`)

**Out-of-order policy:**

Django does not enforce migration ordering at runtime. The `MigrationLoader` builds the graph based on declared `dependencies`; it does not check that applied migrations are in the order recorded in the graph. There is no out-of-order detection or rejection. (`django/db/migrations/loader.py:274-340`)

---

## Diff and generation (`makemigrations`)

**The autodetector algorithm (`django/db/migrations/autodetector.py`):**

Entry point: `MigrationAutodetector.changes(graph, trim_to_apps, convert_apps, migration_name)` → calls `_detect_changes()` → calls `arrange_for_graph()`. (`autodetector.py:62-72`)

`_detect_changes()` runs in a fixed order: (`autodetector.py:127-231`)

```python
# autodetector.py:182-231 (paraphrased order — each line is a method call)
self.generate_renamed_models()    # must run first
self._prepare_field_lists()
self._generate_through_model_map()
self.generate_deleted_models()
self.generate_created_models()
self.generate_deleted_proxies()
self.generate_created_proxies()
self.generate_altered_options()
self.generate_altered_managers()
self.generate_altered_db_table_comment()
self.create_renamed_fields()     # computes self.renamed_fields dict
self.create_altered_indexes()    # computes self.altered_indexes dict
self.create_altered_constraints()
self.generate_removed_constraints()
self.generate_removed_indexes()
self.generate_renamed_fields()    # emits RenameField ops
self.generate_renamed_indexes()
self.generate_removed_altered_unique_together()
self.generate_removed_fields()
self.generate_added_fields()
self.generate_altered_fields()
self.generate_altered_order_with_respect_to()
self.generate_altered_unique_together()
self.generate_added_indexes()
self.generate_added_constraints()
self.generate_altered_constraints()
self.generate_altered_db_table()
self._sort_migrations()       # topological sort within each app (Python graphlib)
self._build_migration_list(graph)  # split into migration objects, resolve cross-app deps
self._optimize_migrations()     # run MigrationOptimizer on each migration's ops
```

**FK dependency tracking:**

`generate_created_models()` separates FK and M2M fields from the main `CreateModel` body and emits them as separate `AddField` operations, each with `_auto_deps` pointing to the target model's `CREATE` dependency. This ensures the target table exists before the FK is added. (`autodetector.py:649-786`)

`_build_migration_list()` resolves these dependency annotations into actual cross-app migration edges using a chopping algorithm: it iterates apps, collects operations whose deps are satisfied, and "chops" them into a migration. It loops until all ops are placed, switching to `chop_mode` (which forces boundaries) if a full pass produces no progress. (`autodetector.py:297-415`)

**Topological sort within an app:**

`_sort_migrations()` uses Python 3.9's `graphlib.TopologicalSorter` to reorder operations within an app so FK constraints are satisfiable inside the same migration. (`autodetector.py:417-433`)

**Optimization pass:**

`_optimize_migrations()` runs `MigrationOptimizer.optimize()` on each generated migration's operation list. The optimizer is a forward-scan: for each operation, it scans right to find the first reducible pair (using each `Operation.reduce()` method), collapses them, and restarts. It loops until stable. (`autodetector.py:435-451`, `optimizer.py:12-69`)

Example: `CreateModel` + `AddField` on same model → single `CreateModel` with the field included. `CreateModel` + `DeleteModel` on same model → empty list (both eliminated). (`operations/models.py:151-302`)

**Rename handling — heuristic, not explicit:**

Django uses field-signature comparison to detect potential renames:

1. **Model rename (`generate_renamed_models()`)**: compares sets of added vs. removed model names within the same app. For each pair, calls `only_relation_agnostic_fields()` to strip FK targets and compare field definitions. If field definitions match, calls `questioner.ask_rename_model(old_model_state, new_model_state)` to confirm interactively. (`autodetector.py:581-647`)

2. **Field rename (`create_renamed_fields()`)**: for each new field key not in old field keys, iterates over old field keys not in new field keys within the same model. Calls `deep_deconstruct()` on both and compares, ignoring `db_column` if the old column name can be preserved. If signatures match, calls `questioner.ask_rename(model_name, old_name, new_name, field)`. (`autodetector.py:1048-1108`)

Both use `InteractiveMigrationQuestioner.ask_rename`:

```python
# django/db/migrations/questioner.py:223-236
def ask_rename(self, model_name, old_name, new_name, field_instance):
  """Was this field really renamed?"""
  msg = "Was %s.%s renamed to %s.%s (a %s)? [y/N]"
  return self._boolean_input(
    msg % (model_name, old_name, model_name, new_name,
        field_instance.__class__.__name__),
    False,
  )
```

And `ask_rename_model`:

```python
# django/db/migrations/questioner.py:238-245
def ask_rename_model(self, old_model_state, new_model_state):
  """Was this model really renamed?"""
  msg = "Was the model %s.%s renamed to %s? [y/N]"
  return self._boolean_input(
    msg % (old_model_state.app_label, old_model_state.name, new_model_state.name),
    False,
  )
```

Non-interactive mode (`NonInteractiveMigrationQuestioner`) defaults `ask_rename` to `False` (treat as add+delete, not rename). (`questioner.py:67-73`)

If the user answers "no", Django emits a `RemoveField` + `AddField` instead, which is a destructive operation with data loss.

**Destructive operation detection and warnings:**

Django does not have a built-in "destructive operation warning" beyond the interactive prompt for rename decisions. It does not warn about `RemoveField`, `DeleteModel`, `AlterField` type changes that cause data loss. The `OperationCategory` enum (`ADDITION`, `REMOVAL`, `ALTERATION`, `SQL`, `PYTHON`, `MIXED`) is available in `--plan` output but does not block execution. (`operations/base.py:7-14`)

**How `ProjectState` is reconstructed from migration graph:**

`MigrationGraph.make_state(nodes, at_end)` generates the forward plan for the given nodes and replays it:

```python
# django/db/migrations/graph.py:315-332
def make_state(self, nodes=None, at_end=True, real_apps=None):
  plan = self._generate_plan(nodes, at_end)
  project_state = ProjectState(real_apps=real_apps)
  for node in plan:
    project_state = self.nodes[node].mutate_state(project_state, preserve=False)
  return project_state
```

`Migration.mutate_state()` calls `operation.state_forwards(app_label, state)` for each operation. (`migration.py:80-92`)

---

## Schema metadata

**Composite unique constraints:**

Django supports composite unique constraints two ways:

1. **Legacy `unique_together`**: declared in `Meta`, represented in migrations as `AlterUniqueTogether`. The operation stores a set of field-name tuples. (`operations/models.py:702-711`)

2. **Modern `UniqueConstraint`**: declared in `Meta.constraints`, represented in migrations as `AddConstraint(model_name, constraint)`. The constraint object (a `UniqueConstraint` instance) is serialized into the migration file. (`operations/models.py:1143-1197`)

`unique_together` is considered deprecated in favor of `UniqueConstraint`; historical migrations still use it. Composite unique constraints over FK fields work correctly because the autodetector emits `AlterUniqueTogether` after the FK `AddField` operations. (`autodetector.py:836-844`)

**Composite indexes:**

Declared in `Meta.indexes` as a list of `Index` instances. In migrations: `AddIndex(model_name, index)` / `RemoveIndex(model_name, name)` / `RenameIndex(model_name, old_index_name, new_index_name)`. Indexes require an explicit `name` attribute; `ModelState.__init__` enforces this:

```python
# django/db/migrations/state.py:777-783
for index in self.options["indexes"]:
  if not index.name:
    raise ValueError(
      "Indexes passed to ModelState require a name attribute. "
      "%r doesn't have one." % index
    )
```

Rename detection for indexes: `create_altered_indexes()` compares `old_indexes` vs `new_indexes` using full `deconstruct()` comparison excluding name; if everything matches except the name, a `RenameIndex` operation is emitted instead of a remove + add pair. (`autodetector.py:1376-1463`)

**Reflection (`inspectdb`):**

Django has `manage.py inspectdb` which introspects a live database using `connection.introspection.get_table_list()`, `get_constraints()`, `get_table_description()`, etc., and emits Python model code. This is a one-shot code generation tool, not used by the migration system itself. (`django/core/management/commands/inspectdb.py:79-121`)

---

## Online-safe / staged migration guidance

**Does Django document or support online-safe patterns?**

Django does not enforce online-safe patterns in its migration runner. The documentation mentions patterns but does not implement them. (Documented patterns are doc-only; not verified in source code.)

**`atomic = False`:**

Implemented as described above. Required for `CREATE INDEX CONCURRENTLY`. The user must set `atomic = False` on the migration and use `AddIndexConcurrently`:

```python
# django/contrib/postgres/operations.py:114-119 (NotInTransactionMixin)
def _ensure_not_in_transaction(self, schema_editor):
  if schema_editor.connection.in_atomic_block:
    raise NotSupportedError(
      "The %s operation cannot be executed inside a transaction "
      "(set atomic = False on the migration)." % self.__class__.__name__
    )
```

**`RunPython`:**

`RunPython` calls user-supplied Python callables with `(apps, schema_editor)`. It receives `from_state.apps` — the historical frozen app registry at the point just before this migration — so the callable gets historically-correct model classes rather than the current production models. (`operations/special.py:187-199`)

```python
# django/db/migrations/operations/special.py:187-199
def database_forwards(self, app_label, schema_editor, from_state, to_state):
  from_state.clear_delayed_apps_cache()
  if router.allow_migrate(...):
    self.code(from_state.apps, schema_editor)
```

**`SeparateDatabaseAndState`:**

Takes `database_operations` and `state_operations` as separate lists. State is mutated only by `state_operations`; the database DDL is driven by `database_operations`. Used when the Python model state and database reality need to diverge temporarily (e.g., adding a column to the DB before updating the ORM model, or when using tools that manage schemas outside Django). (`operations/special.py:6-61`)

**Third-party tooling:**

`django-migration-linter` and `django-squash` exist as community projects for enforcing safe migration practices and squashing, respectively. These are not part of Django core. (Third-party, not verified in source.)

---

## Failure modes

**What happens mid-migration on failure?**

- **Transactional migration** (`atomic = True`, PostgreSQL): if an exception is raised during `migration.apply()`, the outer transaction is rolled back in `SchemaEditor.__exit__`. No row is written to `django_migrations`. The database is left in the pre-migration state. (`django/db/backends/base/schema.py:167-172`)

- **Non-transactional migration** (`atomic = False`): if an exception is raised, any DDL already executed against the database persists. No row is written to `django_migrations`. There is no cleanup, no partial-apply marker, and no recovery path built into Django. The user must manually fix the database state (e.g., drop partial indexes, remove partially-added columns) before retrying. (`django/db/migrations/executor.py:241-266`)

**How is partial apply recorded?**

It is not recorded. The `django_migrations` table is all-or-nothing. (`django/db/migrations/recorder.py:99-107`)

**Rollback semantics:**

Django's rollback is transactional DDL rollback (PostgreSQL-native). Django does not run the "down" migration automatically on failure; that requires the user to explicitly run `manage.py migrate app_label <previous_migration>`, which calls `migration.unapply()`. There is no automatic "rollback on error" path. (`django/db/migrations/executor.py:279-292`)

---

## Historical model handling

**How `ProjectState` and historical models work:**

Each migration operation receives both `from_state` (the `ProjectState` before this operation) and `to_state` (after). For `RunPython`, `from_state.apps` is a `StateApps` instance containing frozen historical model classes:

- `StateApps` is a subclass of the global `Apps` registry. It is constructed from `ModelState` objects, not from live Django model classes. (`django/db/migrations/state.py:622-663`)
- `ModelState.render(apps)` dynamically creates a model class using `type(self.name, bases, body)` where `body["__module__"] = "__fake__"`. (`state.py:960-988`)

The key implication: when user code in `RunPython` calls `apps.get_model("myapp", "MyModel")`, it gets a historically-correct model class frozen at the state just before the migration runs. This class has the exact field set of that migration point, not the current production field set. This allows safe data migrations even when field names have changed since.

**`__fake__` models:**

The `__module__ = "__fake__"` marker is set on dynamically rendered historical model classes. (`state.py:983`)

```python
# django/db/migrations/state.py:981-984
body = {name: field.clone() for name, field in self.fields.items()}
body["Meta"] = meta
body["__module__"] = "__fake__"
```

This prevents Django's model metaclass from treating them as real registered models and polluting the global app registry.

**`HistoricalRecords`:**

`HistoricalRecords` is from the third-party `django-simple-history` package, not Django core. (Not verified in source; labelled accordingly.)

**Why historical models matter for Djogi:**

Djogi has deferred `HistoricContext<'a>`. Django's design shows the two invariants that make historical models safe:
1. The `from_state` must represent the schema *before* this migration's DDL runs — not the current production schema.
2. Model classes must be re-rendered from `ModelState` (not imported from the live codebase) so field changes after the migration was written don't corrupt data migration logic.

Without this, a data migration that was correct when written (operating on e.g., `old_field`) would silently break if the field was later renamed and the migration re-run (as in a fresh database setup).

---

## Lessons for Djogi

### Adopt

- **Topological sort with `graphlib.TopologicalSorter` for within-app ordering** (`autodetector.py:417-433`): Django sorts operations within an app using Python's stdlib topological sorter before building migrations. Djogi's diff engine should do the same to ensure FK/index creation order is correct inside a single migration file pair.

- **Two-phase rename detection** (`autodetector.py:581-647`, `1048-1108`): Compare field signatures (ignoring FK targets using `only_relation_agnostic_fields`) before asking about renames. Djogi's explicit `#[field(renamed_from = "...")]` approach is strictly better for CI (no interactive prompt needed), but the signature-comparison step is still valid for validating that the rename is consistent.

- **Separate state-forwards from database-forwards** (`migration.py:80-92`, `operations/base.py:78-104`): Django's clean separation between `state_forwards()` (pure in-memory) and `database_forwards()` (actual DDL) allows plan simulation without touching the database. Djogi should maintain this pattern in its operation model.

- **Historical model construction via frozen `ModelState`** (`state.py:736-988`): For Djogi's eventual `HistoricContext<'a>`, freeze the schema state at the point just before each migration and expose it for use in data migration Rust closures. The `__module__ = "__fake__"` pattern prevents contamination of the live type registry.

- **`deferred_sql` pattern for indexes** (`schema.py:161`, `169`): Django defers index creation to after the main DDL block but before the transaction commits. This avoids index creation failures from partial table state during the migration body. Djogi's runner should apply indexes after the main DDL segment completes.

- **`detect_soft_applied()` for `--fake-initial`** (`executor.py:310-413`): For baseline adoption, introspect the live database for table/column existence rather than relying solely on the ledger. Djogi's `baseline` command can use a similar strategy.

- **Chopping algorithm for cross-app dependency resolution** (`autodetector.py:297-415`): The iterative "chop" approach for resolving cross-app dependencies (loop over apps, collect operations whose deps are satisfied, form a migration, repeat) is sound. Djogi's differ should use a similar pass when multiple model modules have FK relationships.

- **`MigrationOptimizer` pattern** (`optimizer.py:1-69`): Post-generation optimization reduces noise in generated migrations. For Djogi, this is especially useful when multiple field changes on the same table can be collapsed into a single `ALTER TABLE`.

- **`SeparateDatabaseAndState`-equivalent for Djogi** (`operations/special.py:6-61`): An escape hatch where the user writes the SQL manually and separately declares what state change it represents. Djogi should offer a similar mechanism for migrations that cannot be auto-generated (e.g., custom rewrite of a column type).

### Reject

- **No checksum on migration files** (`recorder.py` — no checksum field): Django silently accepts edited migration files post-apply. Djogi's ledger already stores checksums and validates them. Maintain that. The absence in Django has caused real production incidents where people edited applied migrations.

- **No advisory lock** (`executor.py` — no lock): Django's no-lock approach leads to race conditions with concurrent `migrate` runs. Djogi's `pg_advisory_lock(x'DJOGMIGR'::bigint)` is the correct approach for a Postgres-only system.

- **No partial-apply tracking** (`recorder.py:99-107`): Django's binary applied/not-applied state makes non-transactional migration failures silent and hard to recover. Djogi's ledger column for partial-apply state is the right extension.

- **No out-of-order enforcement** (`loader.py:274-340`): Django applies migrations in any order as long as declared dependencies are met, without checking for out-of-order application vs. historical state. Djogi's explicit out-of-order allow/reject modes (dev vs. CI/prod) are better.

- **Heuristic rename detection requiring interactive prompts** (`questioner.py:223-245`): In non-interactive mode, renames default to `False`, silently generating a destructive add+delete. Djogi's `#[field(renamed_from = "...")]` explicit annotation eliminates this entire class of ambiguity and is safe in automated pipelines.

- **`unique_together` as a legacy path** (`operations/models.py:702-711`): Django maintains two mechanisms for composite unique constraints. Djogi should use a single, first-class `UniqueConstraint` model from day one.

### Defer

- **`squashmigrations`-equivalent**: Django's `squashmigrations` compresses a range of applied migrations into a single replacement. Revisit once Djogi's migration count grows large enough to warrant it. The `replaces` mechanism in `migration.py:40-41` is the key design point.

- **`run_before` dependency attribute** (`migration.py:37`): Allows a migration to declare it must run before another app's migration. Useful for plugin/extension architecture. Defer until Djogi has a multi-crate extension model.

- **Router-based `allow_migrate` support** (`operations/base.py:148-158`): Django allows each operation to check if it should apply to the current database alias. Djogi is Postgres-only so this is not needed now; revisit if multi-database support is added.

- **Full `HistoricContext` (frozen historical models)**: Critical for correct data migrations, but can be deferred until `RunPython`-equivalent (Rust closure migrations) is designed. The invariant to remember: the model type exposed to the data migration closure must reflect the schema *before* this migration's DDL executes.

### Surprises (flag for `13-gap-analysis-vs-current-spec.md`)

- **SURPRISE 1 — No unique constraint on `(app, name)` in `django_migrations`**: Django's ledger has no `UNIQUE(app, name)` — it relies on application logic. Djogi's ledger should have this constraint at the database level to prevent duplicate rows if the advisory lock is ever bypassed. The current Djogi spec does not mention this; add it.

- **SURPRISE 2 — Record timing of apply after `deferred_sql` drains, not after main DDL**: `executor.py:258-262` shows that when `deferred_sql` is non-empty, the migration is recorded *after* `__exit__` (i.e., after deferred SQL runs), not immediately after `migration.apply()` returns. This means the timestamp in the ledger reflects the true completion time including indexes. Djogi's ledger timestamp should follow the same semantic: record after all SQL for the migration has committed.

- **SURPRISE 3 — `atomic = False` does not auto-wrap individual operations that set `operation.atomic = True`**: Django *does* wrap individual atomic operations inside a non-atomic migration in their own `BEGIN`/`COMMIT` blocks (`migration.py:120-133`). This is finer-grained than "the whole migration is non-atomic." Djogi's current spec treats segments as the unit; this is equivalent but should be verified against the segment boundary logic.

- **SURPRISE 4 — `detect_soft_applied()` only looks at `CreateModel` and `AddField` operations** (`executor.py:358-413`): `--fake-initial` does not check constraint or index existence, only table and column presence. Djogi's baseline detection may need to be more thorough if baseline is applied to databases with partially-applied schemas.

- **SURPRISE 5 — No persistence of `ProjectState`**: Django reconstructs `ProjectState` from scratch on every `migrate` or `makemigrations` run by replaying all applied migrations in order. This is O(n) in migration count. Djogi's `schema_snapshot.json` breaks this O(n) dependence and is a significant performance/correctness improvement at scale. This is worth calling out explicitly in the synthesis: Django's lack of a persistent snapshot is a known pain point for large projects.

- **SURPRISE 6 — `only_relation_agnostic_fields()` strips `to=` before comparing for rename detection** (`autodetector.py:113-125`): When detecting model renames, Django ignores where FK fields point. This means a model with a self-referential FK will match a model with a FK pointing elsewhere — potentially causing false-positive rename prompts. Djogi's explicit rename annotation avoids this.

---

## Confidence

| Section | Confidence | Notes |
|---------|-----------|-------|
| Ledger / history table | **high** | Read `recorder.py` in full; model definition is at lines 32-46 |
| Execution / transaction boundaries | **high** | Read `executor.py:241-292`, `schema.py:151-172`, `migration.py:120-133` |
| Non-atomic DDL (`AddIndexConcurrently`) | **high** | Read `contrib/postgres/operations.py:114-172` in full |
| Autodetector algorithm | **high** | Read `autodetector.py:127-451`, `581-647`, `1048-1145` |
| Rename handling (field + model) | **high** | Read questioner.py, autodetector.py rename sections |
| Historical models / `StateApps` | **high** | Read `state.py:622-988`, `special.py:187-199` |
| `--fake` / `--fake-initial` | **high** | Read `executor.py:241-413`, `migrate.py:52-65` |
| Lock strategy | **high** | Confirmed absence — searched all migration files for lock-related calls |
| Checksum | **high** | Confirmed absence — `recorder.py` has no hash field or computation |
| Optimizer | **high** | Read `optimizer.py` in full |
| Graph / DFS traversal | **high** | Read `graph.py` in full |
| `inspectdb` | **medium** | Read command entrypoint; did not read full introspection backend |
| `squashmigrations` | **medium** | Read command grep output; did not read the full command file |
| Online-safe documentation guidance | **low** | Doc-only; Django docs not read in source |
| Third-party tools (`django-migration-linter`, `django-simple-history`) | **low** | Not in this codebase; mentioned from general knowledge |

**Parts of Django not read:**
- `django/db/backends/postgresql/schema.py` — Postgres-specific schema editor overrides (ALTER TABLE, column type changes)
- `django/db/migrations/serializer.py` and `writer.py` — migration file serialization/writing
- `django/core/management/commands/squashmigrations.py` — full squash implementation
- `django/core/management/commands/makemigrations.py` — full makemigrations command
- Test suite (`tests/migrations/`)

---

## Open questions for synthesis

1. **Advisory lock vs. none**: Django's no-lock approach has shipped at massive scale (most Django deployments). Is an advisory lock at the runner level sufficient, or does Djogi need a row-level lock in the ledger table as well (e.g., `SELECT FOR UPDATE` on the ledger row for the migration being applied)?

2. **Segment boundary semantics for non-transactional DDL**: Django's model is "set `atomic = False` on the whole migration and use `AddIndexConcurrently`." Djogi's model is auto-detected segments. Should Djogi also support a manual segment boundary annotation for cases where the auto-detection is insufficient?

3. **Persistent `ProjectState` vs. replay**: Djogi has `schema_snapshot.json`. Does this mean Djogi's differ operates against the snapshot rather than replaying migrations? If so, what happens when the snapshot diverges from the ledger (e.g., after a failed migration)? This needs a clear invariant in the spec.

4. **Partial-apply state machine**: What states should Djogi's ledger row take? (Not applied → In-flight → Applied | Failed-partial)? Django's binary state is clearly insufficient. Should Djogi track a `segment_index` for non-transactional migrations?

5. **Rename detection quality at scale**: Django's heuristic rename detection becomes unreliable when a model is heavily refactored simultaneously (fields added, removed, and renamed in one commit). Djogi's explicit `renamed_from` annotation sidesteps this, but is there a place for an optional hint-based detection for migrations generated from `djogi diff` output?

6. **Cross-crate FK dependencies**: Django solves cross-app FK ordering with the chopping algorithm. If Djogi eventually supports multiple Rust crates owning different model sets with FKs between them, how does the differ handle cross-crate ordering? This is unspecified in the current Djogi spec.
