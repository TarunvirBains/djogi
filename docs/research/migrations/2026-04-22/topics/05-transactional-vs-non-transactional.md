# Topic 05: Transactional vs Non-Transactional Execution

## Executive summary

Ten of the eleven surveyed systems default to running each migration inside its own database transaction. The sole exception is refinery, which in its default (non-grouped) mode executes the migration SQL and the ledger INSERT as two separate transactions — a meaningful gap. Within the per-migration-transaction camp, opt-out syntax is split: Python-based systems use a class attribute (`atomic = False`, `use_transaction() -> Some(false)`) or a runtime configure option (`transaction_per_migration`); Rust and JVM systems use a metadata file (`metadata.toml: run_in_transaction = false`), an XML attribute (`runInTransaction="false"`), or an in-file directive parsed at runtime. Flyway stands alone in auto-detecting statements that cannot run inside a transaction via per-statement regex scanning of every SQL file. No other surveyed system auto-detects. Ledger INSERT placement is almost universally inside the same transaction as the DDL — except refinery (always separate) and Liquibase (always separate, explicit two-commit pattern). Djogi's planned defaults — per-migration transaction, ledger INSERT inside that transaction, file-header directive for opt-out, separate ledger write only when non-transactional — closely match the consensus, with the ledger-write-placement design being the most carefully reasoned among all surveyed tools.

---

## Comparison matrix

| System | Default tx boundary | Opt-out mechanism | Auto-detect non-tx stmts? | Ledger INSERT placement | CREATE INDEX CONCURRENTLY support |
|---|---|---|---|---|---|
| **Django** | Per-migration `BEGIN`/`COMMIT` (Postgres only; conditional on `can_rollback_ddl`) | `atomic = False` on `Migration` class | No — `NotInTransactionMixin` raises error if called inside tx; user must manually set `atomic = False` | Inside migration transaction (same `BEGIN`/`COMMIT`) | Yes — `AddIndexConcurrently` in `django.contrib.postgres.operations`; requires `atomic = False` |
| **Alembic** | Global transaction across all migrations when `transaction_per_migration=False` (default); per-migration when `True` — on Postgres only (`PostgresqlImpl.transactional_ddl = True`) | `transaction_per_migration=True` in `configure()` + `autocommit_block()` per statement | No — `autocommit_block()` is a user-placed escape hatch, not auto-detected | Inside transaction (version update after `migration_fn()` succeeds) | Yes — via `op.get_context().autocommit_block()` wrapping `op.execute("CREATE INDEX CONCURRENTLY ...")` |
| **Flyway** | Per-migration transaction | `-- @executeInTransaction` directive in SQL file (or `executeInTransaction=false` in Java migration); auto-detected if regex matches | **Yes** — `PostgreSQLParser.detectCanExecuteInTransaction` scans every statement | Inside migration transaction for transactional migrations; SEPARATE `success=false` row written on failure for non-transactional | Yes — auto-detected, script flagged non-transactional, runs outside tx |
| **Liquibase** | Per-changeset transaction (when `runInTransaction=true`, the default) | `runInTransaction="false"` attribute on `<changeSet>` | No — no detection; user must set attribute manually | **Separate transaction** — DDL commits, then ledger INSERT in its own commit | No auto-detect; user sets `runInTransaction="false"` manually |
| **Prisma** | Per-migration (inferred from `applyMigrations` RPC semantics and `applied_steps_count` field) | Not surfaced at the TS level; engine handles internally. No user-facing opt-out directive found in the TypeScript clone | Unknown — Rust engine not fully visible; no evidence of auto-detection | Inside migration transaction (row written before DDL, `applied_steps_count` incremented per successful step) | Not documented; no evidence of support in this clone (medium confidence) |
| **Diesel** | Per-migration, via `self.transaction(apply_migration)` closure | `run_in_transaction = false` in `metadata.toml` per migration directory | No | Inside transaction (ledger INSERT inside `apply_migration` closure) | No auto-detect; `metadata.toml: run_in_transaction = false` required; Diesel surfaces the Postgres error as a helpful link |
| **SeaORM** | Per-migration on Postgres (`use_transaction()` returns `None` → backend default → `true`); not transactional on MySQL/SQLite by default | `use_transaction() -> Some(false)` on `MigrationTrait` impl | No | Inside transaction (`insert_migration_record` called inside `transaction.commit()` path) | No auto-detect; `use_transaction() -> Some(false)` required |
| **refinery** | **Per-migration, but migration SQL and ledger INSERT are in SEPARATE transactions** (default non-grouped mode) | No opt-out mechanism exists — framework wraps every `execute` call in its own transaction | No | **Separate from DDL** — two sequential `execute` calls, each its own transaction | None — no mechanism; `CREATE INDEX CONCURRENTLY` inside refinery will fail at Postgres level |
| **cot** | No transaction wrapping — each DDL operation and ledger INSERT are individual statements | No opt-out needed (no wrapping) | No | **Separate from DDL** — DDL executes first, ledger INSERT follows as a separate call | No support |
| **sea-query** | N/A — builder only; emits SQL strings, does not execute | N/A | N/A | N/A | Yes (builder supports `.concurrently()` flag; consumer controls execution context) |
| **SQLAlchemy** | N/A — metadata/DDL layer only (Alembic handles execution) | N/A | N/A | N/A | Yes — `postgresql_concurrently=True` on `Index`; DDL compiler emits `CONCURRENTLY`; non-tx enforcement is caller's responsibility |

---

## The Postgres problem

Postgres supports DDL inside a transaction — `CREATE TABLE`, `ALTER TABLE`, `DROP TABLE`, `CREATE INDEX` (without `CONCURRENTLY`) all roll back cleanly if the transaction aborts. This is a significant advantage over MySQL, where most DDL historically auto-committed. However, a handful of Postgres DDL statements **cannot run inside a transaction at all**:

- **`CREATE INDEX CONCURRENTLY`** — builds an index without taking a full table lock, but requires no enclosing transaction. Postgres raises `ERROR: CREATE INDEX CONCURRENTLY cannot run inside a transaction block` if attempted. Confirmed by the refinery note: `refinery_core/src/runner.rs` discussion, and Django's `NotInTransactionMixin` (`django/contrib/postgres/operations.py:114-120`).
- **`DROP INDEX CONCURRENTLY`** — same constraint. (SQLAlchemy: `lib/sqlalchemy/dialects/postgresql/base.py:2783-2786`; sea-query: `src/backend/postgres/index.rs:91-93`.)
- **`REINDEX CONCURRENTLY` and `REINDEX DATABASE/SCHEMA/SYSTEM`** — Flyway detects these explicitly (`PostgreSQLParser.java:143-144`).
- **`VACUUM`** and **`DISCARD ALL`** — Flyway regex at `PostgreSQLParser.java:145-146`.
- **`ALTER TYPE ... ADD VALUE`** (pre-Postgres 12) — adding a value to an enum type cannot be seen within the same transaction. In Postgres 12+, the restriction is lifted for most cases but the behavior is subtler. Flyway dynamically queries server version to decide (`PostgreSQLParser.java:125-134`). SQLAlchemy's documentation notes this: `lib/sqlalchemy/dialects/postgresql/named_types.py` and the sqlalchemy.md synthesis note.
- **`CREATE DATABASE`**, **`DROP DATABASE`**, **`CREATE TABLESPACE`**, **`CREATE SUBSCRIPTION`** — Flyway's regex catches all of these (`PostgreSQLParser.java:114-137`).
- **`ALTER SYSTEM`** — Flyway also catches this.

Every system that supports Postgres must have an answer to this problem. The surveyed answers:

1. **Auto-detect and strip the transaction** (Flyway only — `PostgreSQLParser.java:114-137`).
2. **Require an explicit opt-out directive** — all other systems.
3. **Provide an escape hatch** that the user must invoke (`autocommit_block()` in Alembic, `-- djogi:no-transaction` in Djogi's plan).
4. **Raise an error at runtime** when a non-transactional statement is attempted inside a transaction — Django's `NotInTransactionMixin` is the clearest example (`django/contrib/postgres/operations.py:114-120`).

The practical implication for Djogi: any migration file containing `CREATE INDEX CONCURRENTLY` (or the other statements above) **must** run outside a transaction wrapper. Djogi's planned `-- djogi:no-transaction` directive in the SQL file header is the right mechanism.

---

## Approaches

### Approach A: Per-migration transaction, opt-out

The overwhelming majority position. Each migration gets its own `BEGIN`/`COMMIT`. Users who need non-transactional DDL opt out on a per-migration basis.

**Django** (`django/db/backends/base/schema.py:151-172`, `executor.py:254-257`):

```python
# executor.py:254-257
with self.connection.schema_editor(
    atomic=migration.atomic
) as schema_editor:
    state = migration.apply(state, schema_editor)
```

The `atomic` attribute on the `Migration` class controls this:

```python
class Migration:
    atomic = True  # default
```

Setting `atomic = False` bypasses the `SchemaEditor.__enter__` wrapping entirely. Django checks `connection.features.can_rollback_ddl` before opening the transaction — on Postgres this is `True`, so migrations are transactional by default.

**Diesel** (`diesel_migrations/src/migration_harness.rs:186-189`):

```rust
if migration.metadata().run_in_transaction() {
    self.transaction(apply_migration)?;
} else {
    apply_migration(self)?;
}
```

Opt-out via `metadata.toml` in the migration directory:

```toml
run_in_transaction = false
```

**SeaORM** (`sea-orm-migration/src/migrator/exec.rs:184-189`):

```rust
fn should_use_transaction(migration: &dyn crate::MigrationTrait, backend: DbBackend) -> bool {
    match migration.use_transaction() {
        Some(v) => v,
        None => backend == DbBackend::Postgres,
    }
}
```

Opt-out: implement `use_transaction()` on the migration struct returning `Some(false)`.

**Flyway** (`DbMigrate.java:277-307`): Per-migration transaction is the default. The SQL file can opt out in two ways — auto-detection (see Approach D) or an explicit directive. The explicit Java migration directive is to override `canExecuteInTransaction()`.

**Prisma**: Per-migration transaction inferred from `applied_steps_count` tracking and the `record_migration_started` / `record_successful_step` pattern. The Rust engine handles transaction boundaries internally; no user-facing opt-out is visible from the TypeScript wrapper (confidence: medium — Rust source was only partially accessible via the prisma-engines patch notes).

### Approach B: Global transaction across all migrations

**Alembic** with default `transaction_per_migration=False` (`runtime/migration.py:145-147`, `372-470`):

```python
# ddl/postgresql.py:84
class PostgresqlImpl(DefaultImpl):
    transactional_ddl = True

# runtime/environment.py:580-582 (configure() option)
# transaction_per_migration defaults to False
```

When `transactional_ddl=True` AND `transaction_per_migration=False` (both the Postgres defaults), Alembic wraps the **entire `run_migrations()` call** in a single transaction. Every migration in one `alembic upgrade head` run either all commits or all rolls back together.

This is maximally safe for all-or-nothing deploys but means `CREATE INDEX CONCURRENTLY` cannot appear in any migration during a batch upgrade unless `transaction_per_migration=True` is set. The `autocommit_block()` escape hatch commits the preceding transaction and runs the DDL outside:

```python
# runtime/migration.py:279-370
def upgrade():
    with op.get_context().autocommit_block():
        op.execute("ALTER TYPE mood ADD VALUE 'soso'")
```

The docstring warns that the migration preceding the block is committed before the operation completes; using `transaction_per_migration=True` is strongly recommended when using `autocommit_block()`.

**Djogi implication**: Alembic's global-transaction default is not the right model for Djogi. It couples unrelated migrations unnecessarily and makes `CREATE INDEX CONCURRENTLY` a footgun. Djogi's per-migration transaction is the cleaner default.

### Approach C: No transaction by default

**refinery** (`traits/sync.rs:85-99`):

In the default (non-grouped) mode, each `execute` call is wrapped by the driver in its own transaction. Both the migration SQL and the ledger INSERT are individual `execute` calls — meaning they run in **two separate transactions**. There is no per-migration `BEGIN`/`COMMIT` that encloses both.

Grouped mode (`set_grouped(true)`) concatenates all SQL and ledger INSERTs into a single execute call, which the driver wraps in one transaction. Noted as unreliable on MySQL (`runner.rs:265-268`).

**cot** (`cot/src/db/migrations.rs:208-212`):

No transaction wrapping at all. Each DDL operation and the ledger INSERT are issued as individual `database.execute_schema(query).await?` calls. No opt-in mechanism exists.

**Djogi verdict on Approach C**: Both refinery and cot pay the price: a crash between migration DDL and the ledger INSERT leaves the schema changed but the ledger unaware. On restart, both will re-attempt the migration and typically fail on "object already exists." This approach is objectively weaker than Approach A for production systems. Djogi should not adopt it.

### Approach D: Auto-detect non-transactional statements

Only **Flyway** does this, via `PostgreSQLParser.detectCanExecuteInTransaction` (`PostgreSQLParser.java:114-137`). The detection logic is regex-based, scanning each SQL statement in the file:

```java
private static final Pattern CREATE_INDEX_CONCURRENTLY_REGEX =
    Pattern.compile("^(CREATE|DROP)( UNIQUE)? INDEX CONCURRENTLY");
private static final Pattern REINDEX_REGEX =
    Pattern.compile("^REINDEX( VERBOSE)? (SCHEMA|DATABASE|SYSTEM)");
private static final Pattern VACUUM_REGEX = Pattern.compile("^VACUUM");
private static final Pattern DISCARD_ALL_REGEX = Pattern.compile("^DISCARD ALL");
private static final Pattern ALTER_TYPE_ADD_VALUE_REGEX =
    Pattern.compile("^ALTER TYPE( .*)? ADD VALUE");
```

If **any** statement in the file matches, the entire migration is flagged `canExecuteInTransaction=false`. Flyway also queries the server version to conditionally apply the `ALTER TYPE ADD VALUE` rule (only non-transactional on Postgres < 12).

When mixing transactional and non-transactional migrations in one deployment batch, Flyway's `mixed` flag controls behavior:

```java
// DbMigrate.java:323-332
if (!configuration.isMixed() && executeGroupInTransaction != inTransaction) {
    throw new FlywayMigrateException(entry.getKey(),
        "Detected both transactional and non-transactional migrations within the same migration group"
        + " (even though mixed is false)...");
}
```

**Assessment**: Flyway's auto-detect is genuinely useful for teams who write SQL migrations directly and might forget to mark them. The regex approach is simple and correct for the common cases. The tradeoff: it runs against every SQL file on every `migrate` invocation, and the regex scanning can mis-classify embedded strings that contain DDL keywords. None of the other nine systems implement this.

**Djogi decision**: Auto-detect is a nice-to-have for Djogi v0.2+, not a v0.1.0 requirement. The planned `-- djogi:no-transaction` directive provides a deterministic, explicit mechanism. Document the known non-transactional statements so users know to add the directive.

---

## Opt-out syntax per system (verbatim)

### Django: `atomic = False`

```python
# django/db/migrations/migration.py (class attribute)
class Migration(migrations.Migration):
    atomic = False

    operations = [
        migrations.AddIndexConcurrently(
            model_name='mymodel',
            index=models.Index(fields=['status'], name='mymodel_status_idx'),
        ),
    ]
```

The `atomic = False` attribute is checked in `executor.py:254-257` and passed to `SchemaEditor`. The `AddIndexConcurrently` operation also carries `atomic = False` at the operation level and will raise `NotSupportedError` if called inside a transaction block (`django/contrib/postgres/operations.py:114-120`):

```python
class NotInTransactionMixin:
    def _ensure_not_in_transaction(self, schema_editor):
        if schema_editor.connection.in_atomic_block:
            raise NotSupportedError(
                "The %s operation cannot be executed inside a transaction "
                "(set atomic = False on the migration)." % self.__class__.__name__
            )
```

### Alembic: `transaction_per_migration` + `autocommit_block()`

```python
# env.py — configure() call
context.configure(
    connection=connection,
    target_metadata=target_metadata,
    transaction_per_migration=True,   # isolate each migration
)

# Inside a migration script
def upgrade():
    with op.get_context().autocommit_block():
        op.execute("CREATE INDEX CONCURRENTLY ...")
```

The `autocommit_block()` context manager sets `isolation_level="AUTOCOMMIT"` on the connection (`runtime/migration.py:344-346`). The docstring warns: "The migration preceding the block is committed before the operation completes."

### Flyway: `-- @executeInTransaction` directive (or auto-detected)

Flyway does not surface a documented single-line SQL directive for opting out in the open-source version. Instead:

1. **Auto-detection** (as documented above) for `CREATE INDEX CONCURRENTLY`, `VACUUM`, etc.
2. **Java migrations**: override `canExecuteInTransaction()` returning `false`.
3. **Configuration**: `flyway.executeInTransaction=false` disables the wrapper globally (not recommended).

The explicit per-file directive `-- @executeInTransaction false` is referenced in Flyway Teams documentation but was not found in the open-source source code reviewed (confidence: medium). The closest source-verifiable per-file control is auto-detection.

### Prisma: (no user-facing opt-out found)

Transaction framing lives entirely inside the Rust schema engine. The TypeScript wrapper does not expose a per-migration transaction opt-out directive. Confidence is medium — the prisma-engines clone was only partially inspected. No `-- prisma:no-transaction` or equivalent was found.

The `--create-only` flag allows generating the migration SQL without applying it, letting users manually edit it (e.g., split into multiple files), which is the recommended workaround for online-safe DDL.

### Liquibase: `runInTransaction="false"` attribute

```xml
<changeSet id="1" author="alice" runInTransaction="false">
    <sql>CREATE INDEX CONCURRENTLY idx_foo ON bar (col)</sql>
</changeSet>
```

Source: `ChangeSet.java:438`, `ChangeSet.java:752, 868-870`.

```java
// ChangeSet.java:752
database.setAutoCommit(!runInTransaction);
// ...
// ChangeSet.java:868-870
if (runInTransaction) {
    database.commit();
}
```

Note: Postgres supports DDL in transactions and `PostgresDatabase` does NOT override `supportsDDLInTransaction()` — it inherits `return true`. So `runInTransaction=true` (the default) works correctly on Postgres. The user must explicitly set `runInTransaction="false"` for `CREATE INDEX CONCURRENTLY`.

### Diesel: `metadata.toml` file in migration directory

```toml
# migrations/20240101_add_idx/metadata.toml
run_in_transaction = false
```

Source: `diesel_migrations/migrations_internals/src/lib.rs:31-42`. The default (when the file is absent or the key is omitted) is `run_in_transaction = true`.

For Rust migrations (not SQL files): `RustMigration::without_transaction()` at `diesel_migrations/src/rust_migrations.rs:305-308`.

### SeaORM: `use_transaction()` method override

```rust
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.execute(
            Statement::from_string(
                DbBackend::Postgres,
                "CREATE INDEX CONCURRENTLY ...".to_string(),
            )
        ).await
    }

    fn use_transaction(&self) -> Option<bool> {
        Some(false)   // opt out of automatic transaction
    }
}
```

Source: `sea-orm-migration/src/lib.rs:40-43`, `exec.rs:184-189`.

### refinery: no opt-out (no transaction wrapping exists to opt out of)

refinery's default non-grouped mode never wraps any migration in a transaction — so there is no wrapping to opt out of. Users cannot opt in to per-migration transactions either. The migration SQL and ledger INSERT each get their own implicit driver-level transaction.

---

## Ledger-INSERT placement

### Same transaction as DDL (correct for transactional migrations)

This is the clear majority pattern:

- **Django**: `executor.py:258-262` — the `record_migration()` call runs after `migration.apply()` returns, inside the same `SchemaEditor` context manager that holds the `BEGIN`/`COMMIT`. The timestamp in the ledger reflects completion after `deferred_sql` drains (SURPRISE 2 in the Django project note).
- **Diesel**: `migration_harness.rs:176-183` — both the migration SQL and the `INSERT INTO __diesel_schema_migrations` are inside the `self.transaction(apply_migration)` closure. Atomically committed or rolled back together.
- **SeaORM**: `exec.rs:254-266` — `migration.up(&txn_manager).await?` then `insert_migration_record(&transaction, ...).await?` then `transaction.commit().await?`. All three steps in one transaction.
- **Flyway** (transactional path): `DbMigrate.java:282` — the `ExecutionTemplate.execute()` call wraps `doMigrateGroup()`, which includes the ledger write inside the same transactional group. The ledger row is written via `schemaHistory.addAppliedMigration(...)` at `DbMigrate.java:419-420` — technically after the migration body but inside the same `ExecutionTemplate` wrapper (one transaction).

  Important nuance from the Flyway note: there is a small window where on a crash between the DDL commit and the `addAppliedMigration` call (the `finally`-adjacent code path), the DDL could be applied without a ledger row. For transactional migrations this is not an issue because the history write is inside the same transaction.

### Separate transaction (two-commit pattern)

- **refinery** (always): `traits/sync.rs:85-99` — migration SQL and ledger INSERT are two separate `execute` calls, each wrapped by the driver in its own transaction. A crash between them orphans the migration.
- **Liquibase** (always): `StandardChangeLogHistoryService.java:399-401` — the ledger INSERT is committed in its own transaction after the DDL transaction commits. Explicit two-transaction topology: `BEGIN; ...DDL...; COMMIT; BEGIN; INSERT INTO databasechangelog; COMMIT;`. The Liquibase project note confirms: "There is a window between the DDL commit and the ledger insert where a crash leaves the DDL applied but the ledger missing the row."
- **cot** (no transaction at all): DDL operations and the ledger INSERT are independent `execute_schema()` calls. No transaction wraps either.

### Conditional — inside if transactional, outside if not

- **Flyway** (non-transactional path): When the migration is non-transactional (either auto-detected or specified), a failure during execution causes Flyway to explicitly write a `success=false` row to the ledger (`DbMigrate.java:258-261`). The ledger write is a separate operation outside any transaction because there is no wrapping transaction. This is the cleanest handling of failure in a non-transactional context among all surveyed tools.

  ```java
  // DbMigrate.java:254-261 — non-transactional failure path
  } else {
      LOG.error(failedMsg + " Please restore backups and roll back database and code!");
      schemaHistory.addAppliedMigration(
          migration.getVersion(), migration.getDescription(),
          migration.getType(), migration.getScript(),
          migration.getChecksum(), executionTime, false /* success=false */);
  }
  ```

- **Prisma** (inferred): `record_migration_started` writes the row before DDL executes; `applied_steps_count` is incremented per successful step; `finished_at` is written on completion. Failed rows have `finished_at IS NULL AND rolled_back_at IS NULL`; after `markMigrationRolledBack`, `rolled_back_at` is set. The row is never deleted. This is the richest post-failure state among all surveyed systems.

**Djogi's plan aligns with the consensus plus Flyway's conditional model**: ledger INSERT inside the transaction when transactional; ledger INSERT as a separate write (with `ON CONFLICT DO UPDATE` for idempotency) after DDL succeeds when non-transactional.

---

## What happens on failure mid-migration?

### Transactional case: full rollback, ledger stays clean

When a migration is wrapped in `BEGIN`/`COMMIT`:

- **Postgres rolls back the entire transaction** — all DDL in that migration is undone. No partial state persists.
- **The ledger INSERT never committed** (it was inside the same transaction). No row appears.
- On the next run, the migration is still "pending" — it can be retried without any cleanup.

This is the cleanest possible failure mode. Django confirms: "No row is written to `django_migrations`. The database is left in the pre-migration state." (`executor.py:241-266`, `schema.py:167-172`). Diesel confirms the same via the `self.transaction(apply_migration)` closure. SeaORM: "If `use_transaction()` is `true` (the Postgres default), the transaction is rolled back — the DDL changes and the ledger INSERT are both reverted."

### Non-transactional case: partial state, explicit ledger update

When a migration is explicitly or implicitly non-transactional:

- **DDL that executed before the failure is committed** — Postgres cannot undo it.
- **The ledger must be written explicitly** to record the failure, or the next run will re-attempt (and likely fail on "object already exists").

Surveyed approaches to recording failure:

| System | What gets recorded |
|---|---|
| **Flyway** | Inserts a row with `success=false` (see `DbMigrate.java:259-261`). `repair` must be run before retrying — it deletes `success=false` rows. |
| **Prisma** | Row already written before DDL starts (via `record_migration_started`). `finished_at IS NULL` means in-flight or failed. `rolled_back_at` set via `markMigrationRolledBack` to acknowledge failure. Row never deleted — becomes audit trail. |
| **Django** | Nothing recorded. `django_migrations` has no row. User must manually fix the DB state before retrying. (`executor.py:241-266`) |
| **Alembic** | Nothing recorded. `alembic_version` not updated. User must manually fix DB and `stamp` to a known revision. |
| **Diesel** | Nothing recorded. No partial-apply state. User must manually fix and retry. |
| **SeaORM** | Nothing recorded. Migration simply not in the ledger. Next run retries. |
| **Liquibase** | Nothing recorded for failures (`MarkChangeSetRanGenerator.java:52-54` returns `EMPTY_SQL` for `FAILED`). Same problem as Django/Alembic. |
| **refinery** | Nothing recorded. Schema changed, ledger silent. Next run re-attempts and likely fails. |
| **cot** | Nothing recorded. No partial tracking. |

**Flyway and Prisma are the only systems that explicitly track failure state in the ledger.** Djogi's plan — write a `failed_at` timestamp or `partial` marker to the ledger row when a non-transactional migration fails — aligns with this approach and improves on the majority.

---

## Retry semantics

| System | After transactional failure | After non-transactional failure |
|---|---|---|
| **Flyway** | Auto-retry safe — migration appears as PENDING. | Must run `flyway repair` first (removes `success=false` row), then retry. |
| **Prisma** | Safe — row is in failed state; `markMigrationApplied` creates a fresh row. | `migrate resolve --applied` or `migrate resolve --rolled-back` to adjust state. |
| **Django** | Safe — no row written; retry runs migration again. | Manual DB cleanup required before retry. No tooling. |
| **Alembic** | Safe — version not updated; retry runs the migration. | Manual DB cleanup + `alembic stamp`. |
| **Diesel** | Safe — no row written. | Manual DB cleanup; no `repair` command. |
| **SeaORM** | Safe — no row written. | Manual DB cleanup; no repair tooling. |
| **refinery** | Safe — no row written. | DDL already applied; retry fails on "object already exists". Manual SQL fix + `Target::Fake` to stamp. |
| **Liquibase** | Safe — no row written. | Manual DB cleanup + `changelog-sync` to stamp. |
| **cot** | N/A (no wrapping) | Same as non-transactional for all cases. |

---

## Multi-statement SQL files: all-or-nothing?

When a migration SQL file contains multiple statements, the question is whether they all succeed or none do.

**With a per-migration transaction**: yes — all statements in the file are inside `BEGIN`/`COMMIT`. A failure anywhere rolls back all previous statements. This is what Django, Diesel, SeaORM, and Flyway (transactional migrations) provide.

**Without a transaction** (non-transactional opt-out): each statement is independently committed as Postgres encounters it. Statements 1 through N-1 may have executed when statement N fails. There is no automatic rollback. This is the user's responsibility.

**Alembic with `transaction_per_migration=False` (default on Postgres)**: all statements from all migrations in the batch are inside one giant transaction. A failure anywhere rolls back the entire batch.

**refinery (grouped mode)**: concatenates all SQL into one `execute` call. The driver wraps it in a transaction — all-or-nothing. The default (non-grouped) mode does NOT guarantee this.

**Liquibase**: each `<changeSet>` is one unit. Within a changeset with `runInTransaction=true`, all changes commit together. Across changesets, each has its own commit cycle.

**Djogi's model** (planned): the per-migration file pair (`NNNN_name_up.sql`) runs inside a single `BEGIN`/`COMMIT`. Every statement in the file is in the same transaction. For non-transactional migrations (opt-out via header directive), each statement is committed as Postgres executes it — users should be advised to keep non-transactional SQL files to a single statement where possible.

---

## Convergence / divergence

### Convergence

- **Per-migration transaction is the overwhelming default.** Nine of eleven systems default to some form of transactional execution per migration (the exceptions are refinery's split-transaction design and cot's no-transaction design).
- **Ledger INSERT inside the migration transaction** is the majority behavior for transactional migrations. Diesel and SeaORM both confirm this is the correct and expected pattern.
- **No auto-detection of non-transactional statements** (Flyway excepted). Every other system requires the user to declare opt-out.
- **No system provides in-migration resumption from a specific statement** after partial failure. Prisma comes closest with `applied_steps_count` but does not implement automatic resumption.

### Divergence

- **Opt-out syntax** varies significantly: Python class attribute (`atomic = False`), Rust method override (`use_transaction()`), TOML file (`run_in_transaction = false`), XML attribute (`runInTransaction="false"`), or (planned for Djogi) an in-file SQL comment directive (`-- djogi:no-transaction`).
- **Ledger placement on non-transactional failure**: Flyway writes an explicit `success=false` row; Prisma tracks `finished_at IS NULL`; all others leave the ledger silent.
- **Global vs per-migration transaction**: Alembic defaults to a single global transaction across all migrations — unique among all surveyed systems.
- **Auto-detect**: Flyway only.

---

## Djogi implications

### Default: per-migration transaction

Each migration pair (`NNNN_name_up.sql`) runs inside a single `BEGIN ... COMMIT`. This matches Django, Diesel, SeaORM, Flyway, and the opt-in Alembic mode. The ledger INSERT is inside the same transaction.

Rationale: on failure, the DB returns to the pre-migration state and the ledger is clean. No repair step needed. Safe to retry immediately.

### Opt-out directive: `-- djogi:no-transaction` in file header

```sql
-- djogi:no-transaction
-- This migration creates an index concurrently and cannot run inside a transaction.

CREATE INDEX CONCURRENTLY idx_orders_status ON orders (status);
```

The directive must appear in the header (first non-blank, non-comment-preamble lines) of the SQL file. The runner scans for it before opening any transaction.

Rationale for text-based directive vs. config file:

- Flyway's regex approach is auto-detection, not a directive.
- Diesel uses a separate `metadata.toml` — additional file to manage.
- Django uses a class attribute — Python-specific.
- Liquibase uses an XML attribute — markup-specific.
- A SQL comment in the file header is the most portable form: the SQL file is self-describing, survives copy-paste, appears in `git log`, and requires no adjacent file.

The closest analogue is Flyway's `-- @executeInTransaction false` (Teams edition) pattern — directive-in-file — which Djogi adopts as a plain SQL comment.

### Ledger INSERT placement

| Migration type | Ledger INSERT placement | Rationale |
|---|---|---|
| Transactional (default) | Inside migration transaction | Roll back together on failure; clean ledger on retry |
| Non-transactional (opt-out) | Separate write after DDL succeeds | DDL cannot be in a transaction; ledger INSERT must be a separate commit |

For non-transactional migrations, use `INSERT ... ON CONFLICT (version) DO UPDATE SET applied_at = EXCLUDED.applied_at` to make the ledger write idempotent — in case the DDL succeeded and the ledger INSERT fails on first attempt (network drop, etc.).

For non-transactional failure, write a `failed_at` marker to the ledger immediately upon catching the error, before propagating. This enables:
- Operators to see the failure without querying DB catalog.
- The `djogi repair` command to have a clear target.
- On retry, the runner can detect the partial state and prompt rather than blindly re-running.

This design synthesizes Flyway's `success=false` row and Prisma's `applied_steps_count` / `finished_at` / `rolled_back_at` into a coherent approach for Djogi's SQL-file-based model.

### Common non-transactional statements — document and advise

Since Djogi does not plan to auto-detect these statements (v0.1.0), document them prominently:

| Statement | Requires `-- djogi:no-transaction`? | Notes |
|---|---|---|
| `CREATE INDEX CONCURRENTLY` | Yes | Primary use case for non-transactional migrations |
| `DROP INDEX CONCURRENTLY` | Yes | Same constraint |
| `REINDEX CONCURRENTLY` | Yes | Postgres 12+ |
| `REINDEX DATABASE` / `REINDEX SCHEMA` | Yes | Cannot run inside a transaction |
| `VACUUM` | Yes | Never runs inside a transaction |
| `ALTER TYPE ... ADD VALUE` | Yes (Postgres < 12) / conditional (Postgres ≥ 12) | Value not visible within same TX in PG < 12 |
| `CREATE DATABASE` / `DROP DATABASE` | Yes | Rarely needed in migrations |
| `ALTER SYSTEM` | Yes | System-level, not schema migration |

For `ALTER TYPE ... ADD VALUE` on Postgres 18 (Djogi's minimum), the restriction is effectively lifted within a transaction block for most cases, but the migration should still be marked non-transactional if the new value needs to be visible in subsequent statements within the same script. Emit a doc comment in generated migrations containing enum additions.

### Auto-detect: defer to v0.2

Flyway's regex-based detection is the only implementation in the field. It is valuable but has false-positive risk (a statement inside a string literal that matches the regex). For Djogi v0.1.0:

- Trust the explicit directive.
- In the runner, detect the presence of `CREATE INDEX CONCURRENTLY` in the SQL text and emit a `tracing::warn!` suggesting the user add `-- djogi:no-transaction` if the directive is absent. This is weaker than Flyway's full auto-detect but provides a safety net.

---

## Open questions

1. **`CREATE INDEX CONCURRENTLY` inside a `-- djogi:no-transaction` migration with multiple statements**: if the file contains both `CREATE INDEX CONCURRENTLY` and a regular DML statement (e.g., `INSERT INTO`), the latter commits immediately. Define clearly: non-transactional files should contain only one DDL statement. Enforce this as a lint warning (`W: non-transactional migration contains multiple statements`).

2. **`ALTER TYPE ... ADD VALUE` on Postgres 18**: Postgres 12 relaxed most restrictions on `ALTER TYPE ... ADD VALUE` inside transactions. Postgres 18 (Djogi's minimum) may further relax this. Document the current Postgres 18 behavior explicitly before v0.1.0 ships. Specifically: is the new enum value visible within the same transaction that added it? If yes, `ALTER TYPE ... ADD VALUE` no longer requires `-- djogi:no-transaction` on Postgres 18. This should be tested.

3. **Advisory lock and non-transactional migrations**: Djogi's advisory lock (`pg_advisory_lock`) is session-scoped, not transaction-scoped. This means the lock persists across the gap between the non-transactional DDL commit and the subsequent ledger INSERT. This is the correct behavior — the lock should not release between the two commits. Verify the lock acquisition point is before the first DDL statement and release is after the ledger INSERT.

4. **Non-transactional migration with a failed ledger INSERT**: if the DDL succeeds but the ledger INSERT fails (network drop, Postgres timeout), the state is: schema changed, ledger silent, advisory lock released. On next run: the runner queries the ledger, sees the migration as pending, and tries to re-run the DDL. For `CREATE INDEX CONCURRENTLY`, re-running will fail with "index already exists" unless the DDL uses `IF NOT EXISTS`. Decide: should Djogi append `IF NOT EXISTS` automatically to non-transactional index creation statements, or require the user to write it?

5. **`djogi repair` command scope for non-transactional partial failures**: after a non-transactional migration fails halfway, the operator needs to: (a) manually fix the DB state, (b) mark the migration as applied or clear the failure marker. The `djogi repair` command should: confirm the user has fixed the DB state, then write a `-- djogi:force-applied` or equivalent ledger update. Define the exact repair flow before implementation.

6. **Retry of a non-transactional migration after partial success**: if a 3-statement non-transactional migration completes statements 1 and 2 then fails on 3, the DB has the effects of 1 and 2. Retrying the whole file will fail on 1 (already exists). Options: (a) always use `IF NOT EXISTS` / `IF EXISTS` in non-transactional migrations, (b) track `applied_steps_count` per Prisma's approach, (c) document that non-transactional migrations should be single-statement. Option (c) is the simplest and avoids a class of problems entirely — recommend it as a style guide enforced by a lint warning.
