# Flyway

## Metadata
- Clone path: `/home/tarunvir/projects/flyway-reference/`
- Commit SHA inspected: `be2566341` ("Bump version to flyway-12.4.0")
- Primary language: Java (Maven multi-module)
- Total LOC of migration-relevant modules: ~5050 LOC across `flyway-core/src/main/java/org/flywaydb/core/internal/schemahistory/`, `flyway-core/src/main/java/org/flywaydb/core/internal/command/`, and `flyway-database/flyway-database-postgresql/src/main/java/` (counted with `find... -name '*.java' | xargs wc -l`).

## Architecture

Flyway's migration system decomposes into four layers (all citations are to files under `/home/tarunvir/projects/flyway-reference/`):

1. **Resolvers** — discover migration resources from the filesystem / classpath / plugins. For SQL: `flyway-core/src/main/java/org/flywaydb/core/internal/resolver/sql/SqlMigrationResolver.java:91`.
2. **Schema-history store** — an abstract `SchemaHistory` plus a JDBC-backed concrete subclass. `flyway-core/src/main/java/org/flywaydb/core/internal/schemahistory/SchemaHistory.java:45`, `flyway-core/src/main/java/org/flywaydb/core/internal/schemahistory/JdbcTableSchemaHistory.java:56`.
3. **Commands** — one class per verb. `migrate` lives in `flyway-core/src/main/java/org/flywaydb/core/internal/command/DbMigrate.java:50`, `repair` in `DbRepair.java:53`, `baseline` in `DbBaseline.java:39`, `clean` in `flyway-core/src/main/java/org/flywaydb/core/internal/command/clean/DbClean.java:44`.
4. **Database-specific adapters** — DDL, locking, parser. For Postgres: `flyway-database/flyway-database-postgresql/src/main/java/org/flywaydb/database/postgresql/PostgreSQLDatabase.java:36`, `PostgreSQLAdvisoryLockTemplate.java:37`, `PostgreSQLConnection.java:33`, `PostgreSQLParser.java:33`.

Key extension seam: `Database#getRawCreateScript(Table, boolean)` is an abstract (`Database.java:362`) that each dialect overrides — this is where the schema-history DDL lives.

Confidence: **high**.

## State model (source-of-truth)

Flyway is **SQL-first and filesystem-first**. The ledger in the database (`flyway_schema_history`) is the record of *what has been applied*; the source of truth for *what migrations exist* is the resolver output (the `ResolvedMigration` set read off disk). The two are reconciled at every command invocation by `MigrationInfoServiceImpl.refresh()` (`flyway-core/src/main/java/org/flywaydb/core/internal/info/MigrationInfoServiceImpl.java:91`).

There is no descriptor/model layer above SQL: a V*__*.sql file is the migration, full stop. `ResolvedMigrationImpl.java:69` exposes `getChecksum()` on the file contents directly.

Applied-state (what's in the DB right now) and execution-history (which rows are in the ledger) are **not** structurally separated — they are the same table. However they are logically teased apart by `MigrationState` (e.g. `SUCCESS`, `FAILED`, `MISSING_SUCCESS`, `FUTURE_FAILED`, `OUT_OF_ORDER`, `OUTDATED`, `SUPERSEDED`, `BASELINE`, `DELETE`), computed per-row from the (applied, resolved) pair at `BaseAppliedMigration.java:156` and `MigrationInfoImpl.java:275-320`.

Confidence: **high**.

## Ledger / history table

**Exact Postgres DDL**, verbatim from `flyway-database/flyway-database-postgresql/src/main/java/org/flywaydb/database/postgresql/PostgreSQLDatabase.java:56-76`:

```java
@Override
public String getRawCreateScript(Table table, boolean baseline) {
  String tablespace = configuration.getTablespace() == null
      ? ""
      : " TABLESPACE \"" + configuration.getTablespace() + "\"";

  return "CREATE TABLE " + table + " (\n" +
      "  \"installed_rank\" INT NOT NULL,\n" +
      "  \"version\" VARCHAR(50),\n" +
      "  \"description\" VARCHAR(200) NOT NULL,\n" +
      "  \"type\" VARCHAR(20) NOT NULL,\n" +
      "  \"script\" VARCHAR(1000) NOT NULL,\n" +
      "  \"checksum\" INTEGER,\n" +
      "  \"installed_by\" VARCHAR(100) NOT NULL,\n" +
      "  \"installed_on\" TIMESTAMP NOT NULL DEFAULT now(),\n" +
      "  \"execution_time\" INTEGER NOT NULL,\n" +
      "  \"success\" BOOLEAN NOT NULL\n" +
      ")" + tablespace + ";\n" +
      (baseline ? getBaselineStatement(table) + ";\n" : "") +
      "ALTER TABLE " + table + " ADD CONSTRAINT \"" + table.getName() + "_pk\" PRIMARY KEY (\"installed_rank\")" + (configuration.getTablespace() != null ? " USING INDEX" + tablespace : "" ) + ";\n" +
      "CREATE INDEX \"" + table.getName() + "_s_idx\" ON " + table + " (\"success\")" + tablespace + ";";
}
```

Column purposes (inferred from write site `JdbcTableSchemaHistory.java:193-195` and read site `JdbcTableSchemaHistory.java:237-245`):

| Column      | Type      | Purpose |
|-------------------|----------------|---------|
| `installed_rank` | `INT NOT NULL` | Monotonic sequence assigned by `SchemaHistory.calculateInstalledRank` (`SchemaHistory.java:231-237`) = `max(installed_rank) + 1`. Primary key. Also doubles as the anchor for `InsertRowLock` (see Execution below). |
| `version`     | `VARCHAR(50)` | Nullable; NULL for repeatable migrations. |
| `description`   | `VARCHAR(200) NOT NULL` | Human description. If source has empty description on a DB that can't store empty strings (Oracle), Flyway substitutes `<< no description >>` (`SchemaHistory.java:46`). |
| `type`      | `VARCHAR(20) NOT NULL` | Enum name from `CoreMigrationType` (SQL, BASELINE, SCHEMA, DELETE, UNDO_SQL, JDBC, etc.). Written as `type.name()` at `JdbcTableSchemaHistory.java:194`. |
| `script`     | `VARCHAR(1000) NOT NULL` | File name (abbreviated via `AbbreviationUtils` at `SchemaHistory.java:219`). |
| `checksum`    | `INTEGER` (nullable) | CRC-32, see "Recovery" below. |
| `installed_by`  | `VARCHAR(100) NOT NULL` | Default is the DB current_user (`Database.java:428-432`). |
| `installed_on`  | `TIMESTAMP NOT NULL DEFAULT now()` | Server-side default. |
| `execution_time` | `INTEGER NOT NULL` | Milliseconds of the migration body. |
| `success`     | `BOOLEAN NOT NULL` | See "Partial-apply handling" below. |

**Indexes / PK strategy:**
- Primary key is `<table>_pk` on `(installed_rank)`, added via `ALTER TABLE... ADD CONSTRAINT... PRIMARY KEY`, not inline. This is deliberate so a single file is shared across dialects that don't allow inline PK, and because some dialects need `USING INDEX` with tablespace.
- A secondary index `<table>_s_idx` on `(success)` is always created. It exists because `DbRepair` filters on `WHERE success = FALSE` (see `getDeleteStatement` at `Database.java:418-426`).
- There is no unique constraint on `(version)` or `(script)` — the schema does not prevent re-inserting the same version as a separate `installed_rank`. This is load-bearing for the repair/delete flow (see Recovery).

Confidence: **high**.

## Execution

### Lock strategy (Postgres)

Postgres uses **session-scoped advisory locks** by default (not row locks, not `LOCK TABLE`). Key code at `flyway-database/flyway-database-postgresql/src/main/java/org/flywaydb/database/postgresql/PostgreSQLAdvisoryLockTemplate.java:37-123`:

```java
private static final long LOCK_MAGIC_NUM =
    (0x46L << 40) // F
        + (0x6CL << 32) // l
        + (0x79L << 24) // y
        + (0x77 << 16) // w
        + (0x61 << 8) // a
        + 0x79; // y
```

The lock key is `LOCK_MAGIC_NUM + discriminator`, where the discriminator is `table.toString().hashCode()` passed in at `PostgreSQLConnection.java:104-106`:

```java
public <T> T lock(Table table, Callable<T> callable) {
  return new PostgreSQLAdvisoryLockTemplate(database.getConfiguration(), jdbcTemplate, table.toString().hashCode()).execute(callable);
}
```

Acquisition is non-blocking with retry: `pg_try_advisory_lock(lockNum)` (session) by default, or `pg_try_advisory_xact_lock(lockNum)` (transactional) when `flyway.postgresql.transactional.lock` is true (`PostgreSQLAdvisoryLockTemplate.java:95-103`, `PostgreSQLConfigurationExtension.java:27-34`). Retry policy is `RetryStrategy` (default driven by `lockRetryCount`) at `PostgreSQLAdvisoryLockTemplate.java:88-93`. Release path is `SELECT pg_advisory_unlock(lockNum)` in a `finally` (`PostgreSQLAdvisoryLockTemplate.java:105-122`); if that returns false, Flyway logs but does not itself abort (unless there was no underlying exception).

Fallback for databases without advisory locks: `InsertRowLock` (`flyway-core/src/main/java/org/flywaydb/core/internal/database/InsertRowLock.java:52`) which inserts a sentinel row with `installed_rank = -100` and relies on PK uniqueness. A 10-minute heartbeat refreshes `installed_on`; expired locks are reaped.

### Transaction boundaries

Default: one migration per transaction. Implemented in `DbMigrate.applyMigrations` at `DbMigrate.java:277-307`:

```java
if (executeGroupInTransaction) {
  ExecutionTemplateFactory.createExecutionTemplate(connectionUserObjects.getJdbcConnection(), database).execute(() -> {
    doMigrateGroup(group, stopWatch, skipExecutingMigrations, true);
    return null;
  });
} else {
  doMigrateGroup(group, stopWatch, skipExecutingMigrations, false);
}
```

Whether the group runs in a transaction is decided per-group in `isExecuteGroupInTransaction` (`DbMigrate.java:309-338`). It walks every migration and calls `resolvedMigration.getExecutor().canExecuteInTransaction()`. The rule is: if any migration reports non-transactional and `mixed=false`, Flyway **throws** rather than silently split. Quote from `DbMigrate.java:323-332`:

```java
if (!configuration.isMixed() && executeGroupInTransaction != inTransaction) {
  throw new FlywayMigrateException(entry.getKey(),
                   "Detected both transactional and non-transactional migrations within the same migration group"
                       + " (even though mixed is false). First offending migration: "
                       +...);
}
```

### Non-transactional DDL auto-detection

Per-statement regex-based in `PostgreSQLParser.detectCanExecuteInTransaction` (`PostgreSQLParser.java:114-137`):

```java
private static final Pattern CREATE_INDEX_CONCURRENTLY_REGEX = Pattern.compile("^(CREATE|DROP)( UNIQUE)? INDEX CONCURRENTLY");
private static final Pattern REINDEX_REGEX = Pattern.compile("^REINDEX( VERBOSE)? (SCHEMA|DATABASE|SYSTEM)");
private static final Pattern VACUUM_REGEX = Pattern.compile("^VACUUM");
private static final Pattern DISCARD_ALL_REGEX = Pattern.compile("^DISCARD ALL");
private static final Pattern ALTER_TYPE_ADD_VALUE_REGEX = Pattern.compile("^ALTER TYPE(.*)? ADD VALUE");
// CREATE/DROP DATABASE, TABLESPACE, SUBSCRIPTION
// ALTER SYSTEM
```

If any statement in a script matches, the whole script is flagged `canExecuteInTransaction=false`. `ALTER TYPE ADD VALUE` is only non-transactional on Postgres < 12 (`PostgreSQLParser.java:125-134`) — Flyway dynamically queries server version to decide. This is surprisingly precise.

### Concurrency posture

Two migrators racing on the same Postgres DB: the advisory lock guarantees only one enters `migrateGroup`. The other's `pg_try_advisory_lock` returns false and it busy-retries per `RetryStrategy`. If the lock holder's session dies, Postgres releases the session advisory lock automatically — the second process will then pick it up. If a process crashes mid-apply of a non-transactional migration, see "Partial-apply" below.

With `isGroup()` true (`DbMigrate.java:99-104`), the lock wraps the *entire* migration run so all history writes commit or roll back atomically. With `isGroup()` false (the default), the lock is acquired per-migration (`DbMigrate.java:149-153`).

Confidence: **high**.

## Recovery

### Checksum algorithm

CRC-32, computed line-by-line over UTF-8 bytes **with line terminators stripped** (`readLine` drops them). This makes the checksum line-ending independent. From `flyway-core/src/main/java/org/flywaydb/core/internal/resolver/ChecksumCalculator.java:41-87`:

```java
public static int calculate(LoadableResource... loadableResources) {
  int checksum;
  checksum = calculateChecksumForResource(loadableResources[0]);
  return checksum;
}

private static int calculateChecksumForResource(LoadableResource resource) {
  final CRC32 crc32 = new CRC32();
  BufferedReader bufferedReader = null;
  try {
    bufferedReader = new BufferedReader(resource.read(), 4096);
    String line = bufferedReader.readLine();
    if (line != null) {
      line = BomFilter.FilterBomFromString(line);
      do {
        crc32.update(line.getBytes(StandardCharsets.UTF_8));
      } while ((line = bufferedReader.readLine()) != null);
    }
  } catch (IOException e) {... }
  return (int) crc32.getValue();
}
```

Notes:
- Only `loadableResources[0]` is hashed; the loop over multiple resources that appears in commented-out bodies is gone in this version (the method reads a single resource).
- BOM is stripped from the first line.
- The CRC-32 value is cast to `int`, so it can be negative in Java and stored in a signed `INTEGER` column.

This is *not* a normalized-SQL hash — whitespace changes, comment changes, and identifier-casing changes **do** change the checksum. Flyway only normalizes line endings and the leading BOM.

### `repair` command

`DbRepair.repair()` (`flyway-core/src/main/java/org/flywaydb/core/internal/command/DbRepair.java:114-155`) wraps the entire operation in one transaction and performs three actions in order:

1. **`removeFailedMigrations`** (`JdbcTableSchemaHistory.java:282-320`) — issues `DELETE FROM <table> WHERE success = FALSE AND (version = ? OR description = ?)` (template at `Database.java:418-426`). Only rows with `success=false` are deleted; successful rows are never physically deleted by repair.

2. **`deleteMissingMigrations`** (`DbRepair.java:157-180`) — for any applied migration whose resolved counterpart has vanished from disk AND whose state is `MISSING_SUCCESS`/`MISSING_FAILED`/`FUTURE_SUCCESS`/`FUTURE_FAILED`, Flyway calls `schemaHistory.delete(applied)`. Despite the name, `delete()` **does not DELETE a row**; it *inserts a new row* of type `DELETE` (`JdbcTableSchemaHistory.java:372-399`):

  ```java
  jdbcTemplate.update(
      database.getInsertStatement(table),
      calculateInstalledRank(appliedMigration.getType()),
      versionObj, appliedMigration.getDescription(), "DELETE", appliedMigration.getScript(),
      checksumObj, database.getInstalledBy(), 0, appliedMigration.isSuccess());
  ```

  So the ledger is append-only for non-failed rows — tombstones are written rather than rows mutated. This is a design choice, and it shows up in how `MigrationState` reads them (`BaseAppliedMigration.java:157-159` returns `SUCCESS` for DELETE rows, effectively masking the original).

3. **`alignAppliedMigrationsWithResolvedMigrations`** (`DbRepair.java:182-218`) — for rows where resolved-vs-applied checksum, description, or type drifted, issue:

  ```java
  // Database.java:379-386
  UPDATE <table>
  SET "description" = ?, "type" = ?, "checksum" = ?
  WHERE "installed_rank" = ?
  ```

  This *is* an in-place mutation (the only place repair physically edits a row). Called at `JdbcTableSchemaHistory.java:364`. Flyway refuses to realign synthetic (`BASELINE`/`SCHEMA`/`DELETE`) rows (`DbRepair.java:194,207`) and skips `UNDONE`/`IGNORED` rows.

**What repair refuses to do:**
- It will never drop/truncate the history table (that's `clean`, gated behind `cleanDisabled`, `DbClean.java:63-66`).
- It never deletes a successful row physically.
- It never re-runs SQL. Repair only touches the ledger.

### Baseline / stamp / fake

`DbBaseline.baseline()` (`DbBaseline.java:88-142`) has three states:

1. History table doesn't exist → create with a baseline marker (`DbBaseline.java:92-96`, which passes `baseline=true` into the create DDL, producing the `INSERT` from `Database.getBaselineStatement` at `Database.java:388-400`). The marker is row `installed_rank=1` with `type='BASELINE'`, NULL checksum, `success=TRUE`.
2. History table exists and *has* a baseline marker → if the requested (version, description) matches, no-op; otherwise fail with a link to the "rebaselining" doc.
3. History table exists with non-synthetic migrations but *no* baseline marker → refuse. Force user to drop the table or use the rebaselining flow.

Flyway does not have a "stamp" or "fake" command in the Django sense — marking an individual migration as applied without running it is only possible via direct INSERT or via editing the history table manually. The closest official mechanism is `configuration.isSkipExecutingMigrations()` (`DbMigrate.java:368-370`) which inserts the history row but skips the body. Used programmatically, not a CLI verb.

### Partial-apply handling

The critical path is `DbMigrate.applyMigrations` (`DbMigrate.java:289-306`):

```java
} catch (FlywayMigrateException e) {
  MigrationInfo migration = e.getMigration();
  String failedMsg = "Migration of " + toMigrationText(...) + " failed!";
 ...
  int executionTime = (int) stopWatch.getTotalTimeMillis();
  migrateResult.putFailedMigration(migration, executionTime);

  if (database.supportsDdlTransactions() && executeGroupInTransaction) {
    LOG.error(failedMsg + " Changes successfully rolled back.");
    migrateResult.markAsRolledBack(group.keySet().stream().toList());
  } else {
    LOG.error(failedMsg + " Please restore backups and roll back database and code!");
    schemaHistory.addAppliedMigration(migration.getVersion(), migration.getDescription(),
                     migration.getType(), migration.getScript(), migration.getChecksum(), executionTime, false);
  }
  throw e;
}
```

Two distinct paths:

- **Transactional migration (most cases, since Postgres supports DDL transactions)**: the throw rolls back everything including any history write. No row is inserted. Recovery is automatic on the next run — the failed migration appears as `PENDING`.
- **Non-transactional migration** (or non-transactional DB): Flyway *does* insert a `success=false` row so the failure is visible on the next run. The user must `repair` (which removes that row) before re-running. If the process crashes *before* reaching the catch block (e.g. SIGKILL), no row is inserted at all — the DB state is ambiguous and Flyway on next invocation sees the migration as `PENDING`. There is **no resumable-step machinery** inside a single script.

Note: `schemaHistory.addAppliedMigration(...)` on the success path (`DbMigrate.java:419-420`) is outside the `try`/`catch`, so an insert failure there bubbles up uncaught. The success insert happens **after** the migration body commits, so there's a small window where the DDL is applied but the ledger doesn't yet say so. On DB crash in that window, replay would attempt to re-execute a migration that already ran. For transactional migrations this isn't an issue because the history write is in the same transaction as the DDL (`applyMigrations` wraps the whole `doMigrateGroup` in a transaction at `DbMigrate.java:282`).

### Out-of-order policy

`outOfOrder` is set on `MigrationInfoServiceImpl` (`MigrationInfoServiceImpl.java:50,77`) and consulted twice:

1. When building context (`MigrationInfoServiceImpl.java:321-334`): if an applied migration's version is ≤ context.lastApplied, it is tagged `outOfOrder = true` on its attributes.
2. When selecting pending migrations in `DbMigrate.migrateGroup` (`DbMigrate.java:243-246`):
  ```java
  boolean isOutOfOrder = pendingMigration.getVersion() != null
      && pendingMigration.getVersion().compareTo(currentSchemaVersion) < 0;
  ```
  A pending migration whose version is *less than* the current max applied version is only scheduled when `configuration.isOutOfOrder()` is true; otherwise it stays `IGNORED`. `DbMigrate.migrateGroup` at `DbMigrate.java:193-197` emits a loud warning: "outOfOrder mode is active. Migration of schema... may not be reproducible."

Once such a migration runs, it sits in the history with `installed_rank > max`, but `version < max applied version`. State resolution ends up at `MigrationState.OUT_OF_ORDER` (`BaseAppliedMigration.java:191-193`) and remains that way forever. The ledger is the record that this migration was applied out of order.

Confidence: **high**.

## Diff and generation

Flyway **does not autogenerate** migrations from a schema/model diff. There is no model layer from which to diff. The resolvers (`SqlMigrationResolver`, `ScriptMigrationResolver`, and the Java migration resolver) only *read* user-authored files.

Rename handling: none. There is no rename detection because there is no model — a rename is whatever the user wrote in their SQL.

Destructive-operation detection and gating: the only gate is `cleanDisabled` (`DbClean.java:63-66`):

```java
if (configuration.isCleanDisabled()) {
  throw new FlywayException("Unable to execute clean as it has been disabled with the 'flyway.cleanDisabled' property.");
}
```

There is no pre-flight check for `DROP TABLE` / `DROP COLUMN` inside user migrations. Flyway executes whatever is in the SQL.

Confidence: **high**.

## Schema metadata

Flyway does not own schema authoring. There is no composite-unique-constraint model, no composite-index model, no reflection surface exposed for user consumption. Introspection is limited to the `Schema`/`Table` abstractions used internally for `clean` — e.g. `PostgreSQLSchema.java` enumerates tables for drop. These are not user-facing DDL authoring tools.

Confidence: **high**.

## Online-safe / staged migration guidance

Nothing structural. The only source-level concession to online-safe DDL is the non-transactional auto-detection for `CREATE INDEX CONCURRENTLY` et al. (`PostgreSQLParser.java:114-137`) — Flyway will correctly *not* wrap those in a transaction, but the user is otherwise on their own for two-phase column adds, backfills, check-constraint-validation splits, etc. No documented multi-step guidance inside the source; README and `documentation/` directories mention patterns only in passing.

Confidence: **high** (for the source; **low** for docs, which I only sampled).

## Lessons for Djogi

### Adopt

- **Advisory lock, non-transactional by default.** Flyway's default is session advisory locks on a hashed-magic-number key (`PostgreSQLAdvisoryLockTemplate.java:38-54`). This lets the lock outlive individual transactions, which is correct for non-transactional migrations (you still want mutex even when your migration can't run in a transaction). Djogi's design `pg_advisory_lock(x'DJOGMIGR'::bigint)` matches this posture. *Rationale:* transactional locks release mid-migration if a statement commits out-of-band, which is wrong for CIC. (Citation: `PostgreSQLAdvisoryLockTemplate.java:100-103`.)

- **Primary key on monotonic `installed_rank`, not `version`.** `PostgreSQLDatabase.java:62,74` puts the PK on `installed_rank`. Version is nullable (for repeatables) and deliberately not unique, which allows the append-only DELETE-tombstone pattern (`JdbcTableSchemaHistory.java:372-399`). *Rationale:* append-only ledgers are replay-friendly and auditable; surrogate primary key decouples storage identity from migration identity.

- **Always-on secondary index on `success`.** `PostgreSQLDatabase.java:75` creates `<table>_s_idx` unconditionally. The repair path selects on `success=false` (`Database.java:418-423`), so this index is justified. *Rationale:* cheap, and repair should never scan the full ledger.

- **Per-statement transactional classification, not per-file.** `PostgreSQLParser.detectCanExecuteInTransaction` (`PostgreSQLParser.java:114-137`) inspects each statement; the file is non-transactional iff *any* statement demands it. *Rationale:* a file containing `CREATE INDEX CONCURRENTLY` + an `INSERT` must run wholly outside a transaction. This is simpler and more correct than trying to auto-split within a file.

- **Refuse to silently mix transactional and non-transactional migrations.** `DbMigrate.java:323-332` throws. *Rationale:* the alternative (silent split) surprises users at the worst time.

- **Keep `installed_on` as a server-side `DEFAULT now()`.** `PostgreSQLDatabase.java:69`. *Rationale:* avoids client-clock skew.

- **Hash is line-ending independent.** `ChecksumCalculator.java:63-87` uses `readLine()`, which strips `\n`/`\r\n`. *Rationale:* Windows/Unix users both check in, checksums shouldn't change on `core.autocrlf`.

### Reject

- **CRC-32 as the checksum.** Two reasons, both source-cited. First, CRC-32's collision domain is 2³² (`ChecksumCalculator.java:64,86`) — accidental collisions are rare but not astronomically so in a project with thousands of migrations. Second, CRC-32 is not a cryptographic hash; a malicious edit can be crafted to preserve the checksum. For Djogi, use SHA-256 (or at minimum BLAKE3) truncated to a fixed-width `BYTEA`.

- **Signed `INTEGER` for the checksum column.** `PostgreSQLDatabase.java:67` declares `"checksum" INTEGER` and stores a Java `int` that can be negative (`ChecksumCalculator.java:86`: `(int) crc32.getValue()`). Storing hashes as signed integers is a footgun: comparing across languages requires knowing the signedness. For Djogi, use `BYTEA` or `CHAR(64)` hex.

- **In-place UPDATE on repair-align.** `JdbcTableSchemaHistory.java:363-368` mutates an existing row when checksums drift after an edit. This silently rewrites history. For Djogi, prefer a log-structured approach: insert a new row of type `CHECKSUM_UPDATE` or `RESOLVED_CHECKSUM_CHANGED` with a back-reference. Operators should be able to see that a checksum was realigned and when.

- **`DELETE` as a pseudo-type written back as an INSERT with `success = <original>`.** `JdbcTableSchemaHistory.java:388-394` — the DELETE tombstone copies the *original* `success` flag. This means a `DELETE` row for a failed migration has `success=false`, which confuses every count-based query you might want to run on the ledger. For Djogi, DELETE tombstones should have a dedicated `success=true` (the deletion itself succeeded) and a separate column pointing at the tombstoned installed_rank.

- **`outOfOrder` as a boolean configuration flag without per-migration opt-in.** `DbMigrate.java:193-197` issues a warning and proceeds globally. For Djogi, out-of-order should be per-migration metadata (e.g. a `-- djogi: out-of-order=allow` pragma) so that some migrations can be reordered and others cannot.

- **No sentinel wall between "schema history" and "migration authoring."** Flyway stores `BASELINE`, `SCHEMA`, `DELETE` rows in the same table as regular migrations and filters by `type.isSynthetic()` at query time (e.g. `SchemaHistory.java:87-93`, `DbRepair.java:162-166`). For Djogi, synthetic/event rows should live in a separate `_djogi_events` table; the ledger is just applied migrations.

### Defer

- **Session vs transactional advisory lock toggle.** Flyway exposes `flyway.postgresql.transactional.lock` (`PostgreSQLConfigurationExtension.java:27`). Revisit when we have a concrete scenario (e.g., cancellable migrations) that benefits from transactional locks.

- **InsertRowLock fallback.** Useful only on databases without advisory locks; Djogi is Postgres-only, so skip. Revisit only if we add a second backend.

- **Group mode (atomic multi-migration transactions).** `DbMigrate.java:99-104`. Has real value for "deploy N migrations, all-or-nothing" but complicates the mental model. Defer until a concrete ops request.

### Surprises

1. **`installed_rank` does double duty as a lock key.** `InsertRowLock` writes `installed_rank = -100` (`InsertRowLock.java:161-171`) into the history table itself. This conflates "ledger" and "mutex" in storage. Postgres escapes this because it uses advisory locks; but the choice to leave this in the abstract `SchemaHistory` API leaks into the DDL.

2. **The `BASELINE` INSERT is generated by `String.format` with `%s`.** `Database.java:388-400` takes the JDBC parameterized INSERT, replaces `?` with `%s`, and then `format`s in unescaped string literals. This is technically a SQL-injection vector if `baselineDescription` contains a single quote. The abbreviation utility limits length but doesn't escape. (`Database.java:391-394`.) For Djogi: always use parameterized binds, even for initial setup.

3. **Checksum is `Nullable` but the write path treats it specially.** `JdbcTableSchemaHistory.java:191` uses `JdbcNullTypes.IntegerNull` sentinel because JDBC parameter binding of Java `null` needs type info. Small design tax that bleeds into storage layer.

4. **Postgres `SELECT` path has a `/*NO LOAD BALANCE*/` hint** (`PostgreSQLDatabase.java:125`) — a workaround for pgpool read/write split. Not strictly bad, but worth knowing that Flyway encodes knowledge of specific middlewares into DDL-adjacent code.

5. **`schemaHistory.delete(...)` inserts rather than deletes.** The naming in `SchemaHistory` is adversarial (`SchemaHistory.java:187` javadoc says "Update the schema history to mark this migration as DELETED", then the implementation INSERTs). For Djogi: name methods by mechanism (`insertTombstone`), not intent.

6. **The "mark schema creation" type is `SCHEMA`, not a separate table.** `SchemaHistory.addSchemasMarker` (`SchemaHistory.java:140-148`) inserts a row with `version=NULL, type=SCHEMA`. This is how Flyway tracks which schemas it created so `clean` knows what to drop. It's a surprising overloading — the ledger is a mix of applied-migration rows *and* "things Flyway did to the database."

7. **Description abbreviation is applied at write time.** `SchemaHistory.java:217,219` abbreviates description and script before INSERT. This means the stored row doesn't faithfully reproduce the source file name if it's >200 chars. Not a bug, but a gotcha for tooling that compares ledger to disk.

Confidence: **high**.

## Confidence

| Section | Level | Notes |
|---------|-------|-------|
| Architecture | high | Read all module entry points. |
| State model | high | Read SchemaHistory, JdbcTableSchemaHistory, MigrationInfoImpl, BaseAppliedMigration. |
| Ledger DDL | high | Verbatim quote from PostgreSQLDatabase.java. |
| Execution — lock | high | Read PostgreSQLAdvisoryLockTemplate, PostgreSQLConnection, InsertRowLock. |
| Execution — tx boundaries | high | Read DbMigrate in full. |
| Execution — non-tx DDL | high | Read PostgreSQLParser. |
| Execution — concurrency | medium | Inferred from advisory-lock primitives and retry strategy; no tests read. |
| Recovery — checksum | high | Read ChecksumCalculator in full. |
| Recovery — repair | high | Read DbRepair in full. |
| Recovery — baseline | high | Read DbBaseline in full. |
| Recovery — partial apply | high | Read DbMigrate.applyMigrations catch block. |
| Recovery — out-of-order | high | Read MigrationInfoServiceImpl.updateContextFromAppliedVersionedMigrations. |
| Diff and generation | high | Absent from source (confirmed). |
| Schema metadata | high | Absent from source (confirmed). |
| Online-safe | medium | Parser detection verified high; broader guidance is low (docs only, not read). |

## Open questions

1. **Repeatable migration ordering under `outOfOrder`.** How does Flyway decide which repeatable re-runs when several have outdated checksums? `MigrationInfoServiceImpl.java:283-301` implies "whichever the context puts into `pendingResolvedRepeatableMigrations`"; I didn't trace the ordering guarantee.

2. **What happens if `pg_advisory_unlock` returns false but there was no exception?** `PostgreSQLAdvisoryLockTemplate.java:108-110` throws. I did not find a test exercising this path, so I'm not sure whether it occurs in practice (it might if someone else released the lock out of band).

3. **Is `installed_on` ever read back for ordering?** `getSelectStatement` orders by `installed_rank`, not `installed_on`. But some info-output paths sort by date — I didn't verify whether those paths re-sort client-side.

4. **Behaviour of `repair` when the history table is locked by another process.** `DbRepair.repair()` (`DbRepair.java:122`) wraps in an ExecutionTemplate but I didn't see a `schemaHistory.lock(...)` around it, unlike `DbMigrate`. I would expect repair to race with a concurrent migrate. Needs a test read.

5. **Checksum on `BufferedReader` with 4096-byte buffer** — is there any observable difference in checksum for files that start with unusual characters (e.g., a lone `\r`)? The `do/while` structure at `ChecksumCalculator.java:71-77` reads the first line before the BOM filter on subsequent lines; BOM is only stripped from the first line. I would want a test to confirm.

6. **How does Flyway represent "migration previously applied on a different schema name"?** The hash includes only file content, but the applied-row's `script` column records the file name. If the file is renamed, repair updates the row — but the history table is keyed by `installed_rank`, not `script`, so reconciliation relies on version matching. Edge case: repeatable migrations where description changes. Worth a test.

7. **Maven/plugin-authored migrations** (Java migrations) — their checksums come from `JavaMigration.getChecksum()` which users implement themselves (`BaseJavaMigration.java:94`). This is a trust boundary (user-controlled checksum) that I did not investigate further.
