# Liquibase

## Metadata
- Clone path: `/home/tarunvir/projects/liquibase-reference/`
- Commit SHA inspected: `1d7330406e1bfc3648ba651a4b3b4fe495cbd1a8`
- Primary language: Java (with some Groovy in tests)
- Migration-relevant modules (combined LOC of `lockservice`, `changelog`, `command/core`): ~19,183 lines across roughly 150 `.java` files. `ChangeSet.java` alone is 1,762 lines; `StandardLockService.java` 545; `StandardChangeLogHistoryService.java` 540; `DatabaseChangeLog.java` 1,428.

## Architecture
Source layout follows a clean "state / service / command / sql-generator" split, all in `liquibase-standard`:

- `liquibase-standard/src/main/java/liquibase/changelog/` — domain model. `DatabaseChangeLog.java` (the parsed in-memory changelog), `ChangeSet.java` (one unit of migration), `RanChangeSet.java` (row materialised from `DATABASECHANGELOG`), `StandardChangeLogHistoryService.java` (the ledger read/write), `AbstractChangeLogHistoryService.java` (base class with `upgradeChecksums`, `replaceChecksum`, `replaceFilePath`).
- `liquibase-standard/src/main/java/liquibase/changelog/filter/` — pluggable `ChangeSetFilter` implementations. `ShouldRunChangeSetFilter`, `NotRanChangeSetFilter`, `ContextChangeSetFilter`, `LabelChangeSetFilter`, `DbmsChangeSetFilter`, `UpToTagChangeSetFilter`, `IgnoreChangeSetFilter`, `CountChangeSetFilter`. Filters compose via `ChangeLogIterator` (`changelog/ChangeLogIterator.java`).
- `liquibase-standard/src/main/java/liquibase/changelog/visitor/` — `UpdateVisitor`, `ChangeLogSyncVisitor`, `ValidatingVisitor`, `RollbackVisitor`. Classic visitor over filtered changesets.
- `liquibase-standard/src/main/java/liquibase/lockservice/` — `StandardLockService.java`, `LockServiceImpl`, `OfflineLockService`, `MockLockService`. One lock service per `Database` instance via `LockServiceFactory`.
- `liquibase-standard/src/main/java/liquibase/command/core/` — each user-facing command is an `AbstractCommandStep` implementation: `UpdateCommandStep`, `UpdateSqlCommandStep`, `ChangelogSyncCommandStep`, `ClearChecksumsCommandStep`, `ValidateCommandStep`, `ReleaseLocksCommandStep`, `TagCommandStep`, `DiffCommandStep`, `DiffChangelogCommandStep`, `GenerateChangelogCommandStep`, plus rollback variants.
- `liquibase-standard/src/main/java/liquibase/statement/core/` and `.../sqlgenerator/core/` — the two-layer approach: a `SqlStatement` is a database-agnostic request (e.g. `CreateDatabaseChangeLogLockTableStatement`, `MarkChangeSetRanStatement`, `LockDatabaseChangeLogStatement`), and a `SqlGenerator` emits dialect-specific SQL. Postgres overrides live in `liquibase-standard/src/main/java/liquibase/database/core/PostgresDatabase.java` and in generator-`-ForPostgres.java` variants.
- `liquibase-standard/src/main/java/liquibase/precondition/` — `AbstractPrecondition`, `PreconditionContainer` (in the `.core` subpkg), plus concrete preconditions (`TableExistsPrecondition`, `ColumnExistsPrecondition`, `SqlPrecondition`, `DBMSPrecondition`, `RunningAsPrecondition`, etc.).
- `liquibase-standard/src/main/java/liquibase/change/` — the change "DSL" (`AddColumnChange`, `RenameColumnChange`, `CreateTableChange`, `RawSQLChange`, `SqlFileChange`, …).

**Changeset vs raw SQL**: A `ChangeSet` is always the unit of execution and the unit of history (`liquibase-standard/src/main/java/liquibase/changelog/ChangeSet.java:107-191`). A changeset contains one-or-more `Change` objects. `RawSQLChange` and `SqlFileChange` let a user drop down to literal SQL, but the changeset wrapper — with its `id`/`author`/`filePath` composite identity (`liquibase-standard/src/main/java/liquibase/changelog/ChangeSet.java:117-129`) and its `runInTransaction`, `runAlways`, `runOnChange`, `failOnError`, `dbms`, `contextFilter`, `labels`, `preconditions` — is mandatory. There is no "raw SQL migration" mode that bypasses the changeset identity. Confidence: **high**.

## State model (source-of-truth)
- **Source of truth is the filesystem changelog**: an XML/YAML/JSON/SQL "master" changelog file plus any `<include>`/`<includeAll>` references. Parsed into an in-memory `DatabaseChangeLog` (`liquibase-standard/src/main/java/liquibase/changelog/DatabaseChangeLog.java:410+`).
- **Applied-state is in the database** via the `DATABASECHANGELOG` table — read by `StandardChangeLogHistoryService.getRanChangeSets` (`liquibase-standard/src/main/java/liquibase/changelog/StandardChangeLogHistoryService.java:309-360`). It is queried `ORDER BY DATEEXECUTED ASC, ORDEREXECUTED ASC` (`StandardChangeLogHistoryService.java:379-383`).
- **Execution-history is not separated from applied-state.** `DATABASECHANGELOG` conflates both: the same row is `UPDATE`d when a changeset re-runs (`MarkChangeSetRanGenerator.java:65-84`; `RERAN` branch). `DATEEXECUTED`, `ORDEREXECUTED`, `EXECTYPE`, `DEPLOYMENT_ID`, `MD5SUM` are all overwritten. You cannot ask "how many times has this changeset run?" from the table.
- **`runOnChange`** — if true, a changeset whose checksum differs from the stored one is re-executed. `ShouldRunChangeSetFilter.accepts` returns `"Changeset checksum changed"` (`ShouldRunChangeSetFilter.java:66-68`), and the update branch of `MarkChangeSetRanGenerator` updates the existing ledger row with the new checksum and `EXECTYPE='RERAN'` (`MarkChangeSetRanGenerator.java:65-84`).
- **`runAlways`** (alias: `alwaysRun`) — runs every invocation regardless of checksum or prior execution (`ShouldRunChangeSetFilter.java:63-65`: `"Changeset always runs"`). Parsed at `ChangeSet.java:428`.
- **Checksum validation is skipped for `runOnChange` / `runAlways`** — `ValidatingVisitor.visit` explicitly excludes both from the `invalidMD5Sums` list (`ValidatingVisitor.java:127-133`).

Confidence: **high**.

## Ledger / history table

### `DATABASECHANGELOG`
DDL is assembled programmatically in `liquibase-standard/src/main/java/liquibase/sqlgenerator/core/CreateDatabaseChangeLogTableGenerator.java:47-66`. The raw column-by-column statement is:

```java
return new CreateTableStatement(database.getLiquibaseCatalogName(), database.getLiquibaseSchemaName(), database.getDatabaseChangeLogTableName())
   .setTablespace(database.getLiquibaseTablespaceName())
   .addColumn("ID",..., charTypeName + "(255)",..., new NotNullConstraint())
   .addColumn("AUTHOR",..., charTypeName + "(255)",..., new NotNullConstraint())
   .addColumn("FILENAME",..., charTypeName + "(255)",..., new NotNullConstraint())
   .addColumn("DATEEXECUTED",..., dateTimeTypeString,..., new NotNullConstraint())
   .addColumn("ORDEREXECUTED",..., "int",..., new NotNullConstraint())
   .addColumn("EXECTYPE",..., charTypeName + "(10)",..., new NotNullConstraint())
   .addColumn("MD5SUM",..., charTypeName + "(35)",...)
   .addColumn("DESCRIPTION",..., charTypeName + "(255)",...)
   .addColumn("COMMENTS",..., charTypeName + "(255)",...)
   .addColumn("TAG",..., charTypeName + "(255)",...)
   .addColumn("LIQUIBASE",..., charTypeName + "(20)",...)
   .addColumn("CONTEXTS",..., charTypeName + "(" + getContextsSize() + ")",...)
   .addColumn("LABELS",..., charTypeName + "(" + getLabelsSize() + ")",...)
   .addColumn("DEPLOYMENT_ID",..., charTypeName + "(10)",...);
```

For Postgres this resolves to (reconstructed from the generator — the literal DDL is what `SqlGeneratorFactory` emits at runtime):

```sql
CREATE TABLE public.databasechangelog (
  ID      VARCHAR(255) NOT NULL,
  AUTHOR    VARCHAR(255) NOT NULL,
  FILENAME   VARCHAR(255) NOT NULL,
  DATEEXECUTED TIMESTAMP  NOT NULL,  -- "datetime" in generator; Postgres mapping
  ORDEREXECUTED INT     NOT NULL,
  EXECTYPE   VARCHAR(10) NOT NULL,
  MD5SUM    VARCHAR(35),
  DESCRIPTION  VARCHAR(255),
  COMMENTS   VARCHAR(255),
  TAG      VARCHAR(255),
  LIQUIBASE   VARCHAR(20),
  CONTEXTS   VARCHAR(255),
  LABELS    VARCHAR(255),
  DEPLOYMENT_ID VARCHAR(10)
);
```

**Primary key**: There is none. No column in the `addColumn(...)` chain uses `addPrimaryKeyColumn`, and there is no `ALTER TABLE... ADD PRIMARY KEY` in the generator. Uniqueness of `(ID, AUTHOR, FILENAME)` is enforced by `StandardChangeLogHistoryService` in application code — `RanChangeSet.isSameAs` compares exactly those three fields, and `MarkChangeSetRanGenerator` uses them for its `WHERE` clause (`MarkChangeSetRanGenerator.java:77-80`). The stated `MD5SUM VARCHAR(35)` width enforces the checksum format `V:hex` (see "Recovery" section below).

**Schema migration for existing tables**: `StandardChangeLogHistoryService.init` (`.../StandardChangeLogHistoryService.java:106-297`) detects missing/undersized columns and emits `AddColumnStatement` / `ModifyDataTypeStatement` for each. This is how Liquibase upgrades user databases when a new version adds a column (e.g. `DEPLOYMENT_ID` was introduced this way — see the explicit block at `:242-250`).

Confidence: **high**.

### `DATABASECHANGELOGLOCK`
DDL assembled in `liquibase-standard/src/main/java/liquibase/sqlgenerator/core/CreateDatabaseChangeLogLockTableGenerator.java:23-41`:

```java
CreateTableStatement createTableStatement = new CreateTableStatement(database.getLiquibaseCatalogName(), database.getLiquibaseSchemaName(), database.getDatabaseChangeLogLockTableName())
   .setTablespace(database.getLiquibaseTablespaceName())
   .addPrimaryKeyColumn("ID", DataTypeFactory.getInstance().fromDescription("int", database), null, null, null, new NotNullConstraint())
   .addColumn("LOCKED", DataTypeFactory.getInstance().fromDescription("boolean", database), null, null, new NotNullConstraint())
   .addColumn("LOCKGRANTED", DataTypeFactory.getInstance().fromDescription(dateTimeTypeString, database))
   .addColumn("LOCKEDBY", DataTypeFactory.getInstance().fromDescription(charTypeName + "(255)", database));
```

Resolved for Postgres:

```sql
CREATE TABLE public.databasechangeloglock (
  ID     INT     NOT NULL PRIMARY KEY,
  LOCKED   BOOLEAN   NOT NULL,
  LOCKGRANTED TIMESTAMP,
  LOCKEDBY  VARCHAR(255)
);
```

Primary key is `ID` (`addPrimaryKeyColumn` call). Initialisation inserts a **single row** with `ID=1, LOCKED=false` (`liquibase-standard/src/main/java/liquibase/sqlgenerator/core/InitializeDatabaseChangeLogLockTableGenerator.java:29-32`):

```java
DeleteStatement deleteStatement = new DeleteStatement(..., database.getDatabaseChangeLogLockTableName());
InsertStatement insertStatement = new InsertStatement(..., database.getDatabaseChangeLogLockTableName())
   .addColumnValue("ID", 1)
   .addColumnValue("LOCKED", Boolean.FALSE);
```

The whole locking scheme is "one row, mutate `LOCKED` via `UPDATE`". Confidence: **high**.

## Execution

### Lock strategy
`StandardLockService.acquireLock` (`liquibase-standard/src/main/java/liquibase/lockservice/StandardLockService.java:302-366`) does:

1. `SELECT LOCKED FROM DATABASECHANGELOGLOCK WHERE ID=1` (`SelectFromDatabaseChangeLogLockStatement("LOCKED")` at line 314).
2. If `LOCKED=true`, return `false` (caller waits).
3. Otherwise `UPDATE DATABASECHANGELOGLOCK SET LOCKED=true, LOCKGRANTED=now(), LOCKEDBY='host#desc (ip)' WHERE ID=1 AND LOCKED=false` — the actual statement is produced by `LockDatabaseChangeLogGenerator.generateSql` at `liquibase-standard/src/main/java/liquibase/sqlgenerator/core/LockDatabaseChangeLogGenerator.java:46-51`:

  ```java
  updateStatement.addNewColumnValue("LOCKED", true);
  updateStatement.addNewColumnValue("LOCKGRANTED", new DatabaseFunction(dateValue));
  updateStatement.addNewColumnValue("LOCKEDBY", hostname + hostDescription + " (" + hostaddress + ")");
  updateStatement.setWhereClause(... + " = 1 AND " +... + " = " +...objectToSql(false, database));
  ```

  The `AND LOCKED=false` clause is the entire concurrency-safety story: if two processes race, only one `UPDATE` reports `rowsUpdated == 1`. The loser sees `rowsUpdated == 0` and `acquireLock` returns `false` (`StandardLockService.java:342-346`).
4. `database.commit()` — so the lock ack is visible to other sessions (`StandardLockService.java:347`).

**Wait loop** (`StandardLockService.waitForLock`, `:257-299`): compute a deadline `now + CHANGELOGLOCK_WAIT_TIME*60000` ms (default wait 5 minutes — `GlobalConfiguration.java:83`); call `acquireLock` in a loop, sleeping `CHANGELOGLOCK_POLL_RATE` seconds between attempts (default 10s — `GlobalConfiguration.java:89`). If the deadline passes, throw `LockException("Could not acquire change log lock. Currently locked by " + lockedBy)` with the `LOCKEDBY` string and the `LOCKGRANTED` timestamp.

**Stale locks / crash recovery**: There is **no** automatic timeout on the lock itself. If the process holding `LOCKED=true` crashes, the row stays `true` forever until the deadline expires or someone intervenes. The `LOCKGRANTED` timestamp is informational — nothing in `StandardLockService` compares it to "now". The only recovery paths are:
- The user runs the `releaseLocks` command (`ReleaseLocksCommandStep.java:18-28`), which calls `LockService.forceReleaseLock`, which calls `releaseLock` (`StandardLockService.java:494-497`). `releaseLock` unconditionally `UPDATE`s `LOCKED=false, LOCKGRANTED=null, LOCKEDBY=null WHERE ID=1` (`UnlockDatabaseChangeLogGenerator.java:25-29`). No ownership check.
- The user manually updates the row.

**Init retry loop** (`StandardLockService.init`, `:104-166`) handles the narrow race where two processes both try to create the table at once. Up to 10 iterations, with a random `Thread.sleep(random.nextInt(1000))` between attempts and a `database.rollback()` to recover Postgres's aborted-transaction state (comment at `:153-155`).

**Postgres specifics**: `init` explicitly rolls back on any failure because "servers like Postgres will not allow continued use of the same connection, failing with a message like 'current transaction is aborted, commands ignored until end of transaction block'" — exact comment at `StandardLockService.java:153-156`. This is Postgres-aware code, not generic.

**Concurrency posture**: Single-writer. The design goal is "only one Liquibase instance modifies the DB at a time"; multi-writer concurrency is explicitly out of scope. Confidence: **high**.

### Transaction boundaries
Per-changeset, not per-changelog. `ChangeSet.execute` (`liquibase-standard/src/main/java/liquibase/changelog/ChangeSet.java:728-934`):

- Line 752: `database.setAutoCommit(!runInTransaction)` — only if `database.supportsDDLInTransaction()`.
- Line 860: `database.executeStatements(change, databaseChangeLog, sqlVisitors)` runs each Change's SQL.
- Line 868-870: `if (runInTransaction) { database.commit(); }` — commits once per changeset.
- Line 925-931: restores autocommit after the changeset, again guarded by `supportsDDLInTransaction`.

`runInTransaction` defaults to `true` (`ChangeSet.java:438`: `this.runInTransaction = node.getChildValue(null, "runInTransaction", true);`).

### Non-transactional DDL (Postgres-specific)
Postgres *does* support DDL inside a transaction, and `PostgresDatabase` does **not** override `supportsDDLInTransaction()` — so it inherits `return true` from `AbstractJdbcDatabase.java:195`. This means by default every changeset runs inside a transaction on Postgres, which is correct for most DDL but breaks for `CREATE INDEX CONCURRENTLY`, `ALTER TYPE... ADD VALUE`, `REINDEX CONCURRENTLY`, etc. The only escape hatch is to set `runInTransaction="false"` on the changeset manually. No built-in warning, no detection of concurrent-mode statements.

### Ledger write
After a changeset succeeds, `setExecType` is called (`AbstractUpdateCommandStep` path via `UpdateVisitor`). `StandardChangeLogHistoryService.setExecType` (`.../StandardChangeLogHistoryService.java:395-409`) executes `MarkChangeSetRanStatement` and commits. The generator (`MarkChangeSetRanGenerator.java:41-113`):

- For new changesets (non-`ranBefore` exec types — `EXECUTED`, `MARK_RAN`): `INSERT` all 14 columns.
- For `ranBefore` exec types (`RERAN`): `UPDATE` the existing row matched by `(ID, AUTHOR, FILENAME)`, overwriting `DATEEXECUTED`, `ORDEREXECUTED`, `MD5SUM`, `EXECTYPE='RERAN'`, `DEPLOYMENT_ID`, `COMMENTS`, `CONTEXTS`, `LABELS`, `LIQUIBASE`, `DESCRIPTION`.
- For `FAILED` / `SKIPPED`: `return EMPTY_SQL` (`:52-54`) — **failed and skipped changesets are NOT recorded in the ledger**. This is a significant property: if a changeset fails in the middle of a deployment, the ledger simply has no row for it.

The ledger insert is committed in its own transaction (`StandardChangeLogHistoryService.java:399-401`). So the actual transaction topology per changeset is: `BEGIN;...DDL...; COMMIT; BEGIN; INSERT INTO databasechangelog; COMMIT;` — two transactions if `runInTransaction=true`. There is a window between the DDL commit and the ledger insert where a crash leaves the DDL applied but the ledger missing the row. Confidence: **high** (read straight from source).

## Recovery

### Checksum algorithm
Defined in `liquibase-standard/src/main/java/liquibase/change/CheckSum.java`.

- Storage format: `<version>:<hex-md5>`, e.g. `9:2cdf9876e74347162401315d34b83746` (`CheckSum.java:124-126`, regex at `:39-40`). This is why `MD5SUM VARCHAR(35)` — 1 digit + `:` + 32-char hex = 34, rounded up.
- Input to MD5: `CheckSum.compute(String)` at `CheckSum.java:85-91` applies `StringUtil.standardizeLineEndings` (normalises `\r\n`→`\n` via the regex pipeline), strips Unicode replacement char `�`, then NFC-normalises, then MD5-hashes.
- What gets hashed: in `ChangeSet.generateCheckSum` (`liquibase-standard/src/main/java/liquibase/changelog/ChangeSet.java:396-422`), the builder concatenates `change.generateCheckSum() + ":"` for each `Change` in the changeset, then `visitor.generateCheckSum() + ";"` for each `SqlVisitor`. Each `Change`'s own checksum is assembled from its serialised form (via reflection over `DatabaseChangeProperty` getters) — so the checksum covers the **parsed Change DSL**, not the emitted SQL. Changes to the SQL-generator for your Postgres version will NOT change the checksum.
- Versions (`liquibase-standard/src/main/java/liquibase/ChecksumVersion.java:12-22`): V1–V9, current `V9` since Liquibase 4.22.0. V8→V9 change: V9 excludes `DbmsTargetedChange` instances that don't match the current database (`ChangeSet.java:404-406`).

### `clearChecksums` command
`liquibase-standard/src/main/java/liquibase/command/core/ClearChecksumsCommandStep.java:33-41`. It calls `ChangeLogHistoryService.clearAllCheckSums` — which (in `StandardChangeLogHistoryService.clearAllCheckSums`, `:465-476`) emits:

```sql
UPDATE databasechangelog SET MD5SUM = NULL;
```

No row filtering — every row's MD5SUM is wiped. Then the in-memory `ranChangeSetList` is nulled and `FastCheckService` cache is cleared (`.../StandardChangeLogHistoryService.java:473-475`). On the next `update`, `upgradeChecksums` (`AbstractChangeLogHistoryService.java:66-83`) walks ran-changesets with `NULL` checksums and calls `replaceChecksum(changeSet)` per changeset, which emits `UpdateChangeSetChecksumStatement` — a per-row `UPDATE databasechangelog SET MD5SUM=? WHERE ID=? AND AUTHOR=? AND FILENAME=?`.

So `clearChecksums` is a pure "trust-the-filesystem, recompute-everything" nuke. It does not remove rows or change `EXECTYPE`. Confidence: **high**.

### `update` vs `updateSQL` vs `validate`
- `update`: normal apply path, uses `UpdateCommandStep` → `UpdateVisitor` to execute each un-ran (or should-run) changeset.
- `updateSQL`: same planning, different `Executor`. `UpdateSqlCommandStep` swaps in a `LoggingExecutor` that writes SQL to an `OutputStream` instead of executing it. `StandardLockService.init` detects this at `:109-116` (`executor instanceof LoggingExecutor`) and short-circuits the table-create retry loop. Produces a reviewable SQL script; does not touch the DB.
- `validate`: `ValidateCommandStep.run` is a no-op that prints "No validation errors" (`ValidateCommandStep.java:37-42`). All the real work happens earlier in `DatabaseChangelogCommandStep` / `DatabaseChangeLog.validate` (`DatabaseChangeLog.java:364-394`), which runs a `ValidatingVisitor` that checks duplicate `id::author::file`, missing required attributes, incompatible checksums, and evaluates global preconditions. Throws `ValidationFailedException` on failure.

Confidence: **high**.

### Preconditions
Source: `liquibase-standard/src/main/java/liquibase/precondition/`, `liquibase-standard/src/main/java/liquibase/precondition/core/`.

- Preconditions attach at the changelog level OR the changeset level. In `ChangeSet.execute` (`ChangeSet.java:770-834`) the container's `onFail` / `onError` drives behaviour:
 - `HALT`: throw `MigrationFailedException`.
 - `CONTINUE`: skip the changeset (`execType = SKIPPED`), which (critically) means **no ledger row is written**.
 - `MARK_RAN`: skip but write a ledger row with `EXECTYPE='MARK_RAN'` — used to mask no-op cases without re-checking.
 - `WARN`: just log.
- Concrete preconditions: `TableExistsPrecondition`, `ColumnExistsPrecondition`, `IndexExistsPrecondition`, `ForeignKeyExistsPrecondition`, `PrimaryKeyExistsPrecondition`, `SequenceExistsPrecondition`, `ViewExistsPrecondition`, `RowCountPrecondition`, `TableIsEmptyPrecondition`, `ChangeSetExecutedPrecondition`, `ChangeLogPropertyDefinedPrecondition`, `DBMSPrecondition`, `RunningAsPrecondition`, `SqlPrecondition` (custom SQL returning an expected value), plus the logical combinators `And`/`Or`/`Not` extending `PreconditionLogic`.
- The container itself extends `AndPrecondition` (`PreconditionContainer.java:25`) — so children are AND'ed by default.

Confidence: **high**.

### Baseline / fake / stamp: `changelog-sync`
Defined in `liquibase-standard/src/main/java/liquibase/command/core/ChangelogSyncCommandStep.java`.

What it does (`ChangelogSyncCommandStep.run`, `:55-84`):
1. Init the history table.
2. Build a `ChangeLogIterator` with `NotRanChangeSetFilter` (only un-ran changesets), `ContextChangeSetFilter`, `LabelChangeSetFilter`, `IgnoreChangeSetFilter`, `DbmsChangeSetFilter`.
3. Visit each with `ChangeLogSyncVisitor`, whose `visit` method (`ChangeLogSyncVisitor.java:39-58`) calls `database.markChangeSetExecStatus(changeSet, ExecType.EXECUTED)` — i.e. it writes an `INSERT` into `DATABASECHANGELOG` with `EXECTYPE='EXECUTED'` **without actually running the changes**.

**No safety checks.** Liquibase does not snapshot the schema and warn "hey, this table doesn't exist, you probably shouldn't mark this changeset as applied". It writes the ledger row blind. The `changelog-sync-to-tag` variant stops at a given tag, but the mechanism is the same. There is no explicit "baseline" concept separate from this — `changelog-sync` IS the baseline primitive, combined with hand-picked `<preConditions>` that `MARK_RAN` on existing objects for finer control.

Confidence: **high**.

### Partial-apply handling
There is **no explicit partial-apply state**. If a changeset's DDL commits but the subsequent ledger `INSERT` fails (crash between the two commits — see Execution section), Liquibase on next run sees the changeset as un-ran and re-executes it, which will typically fail on "object already exists" unless the user adds `<preConditions onFail="MARK_RAN">` or splits the changeset. There is no "partial" `EXECTYPE`, no orphan-row detection, no repair workflow. The closest thing is `changelog-sync-to-tag` used as a manual repair tool.

Within a single changeset with `runInTransaction=false`, if some changes succeed and one fails, the ledger gets no row (FAILED/SKIPPED don't write) — `MarkChangeSetRanGenerator.java:52-54` — so from the ledger's perspective, the changeset is still un-ran, and on retry Liquibase will try to re-execute all changes. This is explicitly the user's problem to guard against with preconditions.

Confidence: **high**.

### Out-of-order policy
Not surfaced as a first-class concept. `NotRanChangeSetFilter.accepts` (`NotRanChangeSetFilter.java:18-25`) returns `true` for any changeset whose `(ID, AUTHOR, FILENAME)` triple is not in `ranChangeSets` — regardless of ordering. This means a new changeset inserted *before* already-ran changesets in the changelog file **will be applied next run**, and the execution history (`ORDEREXECUTED`, `DATEEXECUTED`) will faithfully record that it ran later than its neighbours.

The CLI has an `--ignoreOutdatedChangesets` / validator warning path (searched: not found as a first-class filter in this tree — appears to be opt-in Pro behaviour), but the core policy is permissive: out-of-order is allowed silently. Confidence: **medium** (I confirmed the permissive-by-default behaviour from the filter source; the Pro "strict order" option I did not locate in OSS source).

## Diff and generation

- `diffChangeLog` / `generateChangeLog` commands (`DiffChangelogCommandStep.java`, `GenerateChangelogCommandStep.java`) produce real changelog XML/YAML/JSON (or SQL if the output is `.sql`), not just diagnostic reports. `GenerateChangelogCommandStep.run` (`:120-177`) writes the output via `DiffToChangeLog.print(...)`.
- Emitted Change types include `createTable`, `createIndex`, `addColumn`, `dropColumn`, `addForeignKeyConstraint`, `createSequence`, etc. `DiffToChangeLog` does not emit `RenameColumnChange` for renames between snapshots: there is no identity-mapping step that could detect a rename (all diffs are by name). The only place `RenameColumnChange` appears in the diff output path is in `ChangedColumnChangeGenerator.java:195-201` — and it's used as part of a workaround for type conversions that can't be expressed directly (add temp column, copy data, drop original, rename temp to original). **Rename as a semantic rename is not detected by diff**; users must write the `<renameColumn>` / `<renameTable>` changeset by hand.
- The runtime changes `renameColumn` and `renameTable` exist as first-class Change types (`liquibase-standard/src/main/java/liquibase/change/core/RenameColumnChange.java:15-22`, and the analogous `RenameTableChange.java`). They emit `ALTER TABLE... RENAME COLUMN...` via `RenameColumnGenerator` / `RenameTableGenerator`.
- **Destructive-operation gating**: Not found in source. `DropTableChange`, `DropColumnChange`, `DropAllCommandStep` run without confirmation flags in the OSS source. `DropAllCommandStep.java` exists and is destructive by design. No "this changeset is destructive, require --force" detection in the OSS source tree.

Confidence: **high** for the diff/generate mechanics; **medium** for the rename-detection claim (I verified the one place rename appears in diff output and it's a type-conversion workaround, not a semantic-rename detector — but I did not exhaustively scan every snapshot/diff comparator).

## Schema metadata

- **Composite unique constraints**: represented by `AddUniqueConstraintChange` whose statement carries `columnNames` (comma-separated). The generator emits either `ALTER TABLE t ADD UNIQUE (col1,col2)` or `ALTER TABLE t ADD CONSTRAINT name UNIQUE (col1,col2)` (`AddUniqueConstraintGenerator.java:46-58`). Constraint name is user-specified; there's no auto-name-generation for anonymous constraints — the DB picks one.
- **Composite indexes**: `CreateIndexChange` takes a `<column>` list in order; the generator emits a single `CREATE INDEX name ON tbl (col1,col2)`. Naming: user-specified via `indexName`.
- **Reflection / introspection**: yes — `SnapshotGeneratorFactory` + the `snapshot` command (in `liquibase-standard/src/main/java/liquibase/command/core/SnapshotCommand.java`) reads a live database schema into a `DatabaseSnapshot` object, and `GenerateChangelogCommandStep` plus `DiffToChangeLog` turn that snapshot into a changelog. So Liquibase can definitely "read an existing DB and write a changelog for it".

Confidence: **high**.

## Online-safe / staged migration guidance
Nothing built-in. Search findings:

- No detection of `CONCURRENTLY` index patterns.
- No "online migration" helper changes.
- No documented warnings at changeset-execute time about long-running DDL, lock escalation, or Postgres-specific ACCESS EXCLUSIVE behaviour.
- The only Postgres-specific thing touching transaction semantics I found is the `init` retry comment about `current transaction is aborted` (`StandardLockService.java:153-156`) — which is about recovering from a failed lock-table create, not about running online DDL.

A user who wants online-safe DDL today writes multiple small changesets with `runInTransaction="false"` where needed, and manages their own staging via changelog files. Liquibase's posture is "you asked us to run this SQL, we ran it".

Confidence: **high** (for absence — I searched for concurrency / online-safe / advisory lock and found nothing in the core migration path).

## Lessons for Djogi

### Adopt
1. **A `LOCKEDBY` identification string with host + user-supplied description.** `LockDatabaseChangeLogGenerator.java:30-34` uses `hostname + hostDescription + " (" + hostaddress + ")"`. When a lock is stuck, the operator immediately knows which machine to investigate. Djogi's advisory-lock approach will need an auxiliary row or `pg_stat_activity` lookup to provide this, but the user-facing error message format ("Could not acquire change log lock. Currently locked by HOST since DATE") from `StandardLockService.java:291-297` is worth copying.
2. **Dedicated `EXECTYPE` enum values (`EXECUTED`, `RERAN`, `MARK_RAN`, `FAILED`, `SKIPPED`).** Even though Liquibase doesn't write rows for `FAILED`/`SKIPPED`, the distinction between `EXECUTED` (normal apply), `RERAN` (runOnChange fired), and `MARK_RAN` (fake/baseline) is semantically rich. Djogi's `execution_mode` column should distinguish these at minimum, plus a true `FAILED_PARTIAL` state that Liquibase lacks. Source reference: `ChangeSet.java:73-89`, `MarkChangeSetRanGenerator.java:52-54`.
3. **Explicit `DEPLOYMENT_ID`.** Groups changesets from one `update` invocation into a single run (`MarkChangeSetRanGenerator.java:99`). Lets operators query "what landed in that deploy". Cheap column, high operator value. Adopt.
4. **Embedded checksum version prefix (`V:hex` format).** `CheckSum.java:124-126`, `ChecksumVersion.java:12-22`. Lets Liquibase change the algorithm without breaking existing ledgers — the stored version tells the validator which algorithm to use for comparison (`ValidatingVisitor.java:132`, `ShouldRunChangeSetFilter.java:83`). Djogi should do this from day one rather than migrating later.
5. **Allow `runInTransaction=false` per-migration.** `ChangeSet.java:752,868-870,925-931`. Essential for Postgres `CREATE INDEX CONCURRENTLY`. Djogi must support this.
6. **`runOnChange` for view/function-body migrations.** `ShouldRunChangeSetFilter.java:66-68`. Redefinable objects (views, functions, triggers) benefit from "re-apply when source changes" without needing a new migration every time. Adopt but clearly mark as non-additive.

### Reject
1. **Lock table with row update as the concurrency primitive.** `StandardLockService.acquireLock` — depends on `UPDATE... WHERE LOCKED=false` returning `rowCount==1`. This works but has no auto-recovery from crashes; a killed process leaves `LOCKED=true` indefinitely. Djogi's Postgres 18 advisory locks (`pg_try_advisory_lock` / `pg_advisory_unlock`) are strictly better: session-scoped, auto-released on disconnect, no stale-lock problem. **Explicitly do not copy Liquibase's design here.**
2. **Conflating ledger and execution history in one table.** An `UPDATE` on re-run overwrites `DATEEXECUTED`, `ORDEREXECUTED`, `MD5SUM`, `EXECTYPE`, `DEPLOYMENT_ID` (`MarkChangeSetRanGenerator.java:65-84`). You cannot audit "how often did this changeset run, and when?". Djogi should split: a stable `migrations` (applied-state) table plus an append-only `migration_runs` (history) table.
3. **Dropping failed/skipped changesets from the ledger.** `MarkChangeSetRanGenerator.java:52-54` explicitly returns `EMPTY_SQL` for `FAILED` and `SKIPPED`. This is why Liquibase has no partial-apply story: failures leave no trace in the ledger. Djogi MUST record attempts — success or failure — so the crash-between-DDL-commit-and-ledger-commit window becomes recoverable by examining the last attempt.
4. **`clearChecksums` as a single blind `UPDATE... SET MD5SUM=NULL`.** `StandardChangeLogHistoryService.clearAllCheckSums` (`:465-476`). No filtering, no confirmation, no audit trail of what was cleared. Djogi's `repair` command should at minimum log every checksum change and support dry-run and single-migration targeting.
5. **No primary key on `DATABASECHANGELOG`.** Uniqueness of `(ID, AUTHOR, FILENAME)` is enforced purely in application code (`RanChangeSet.isSameAs`, `MarkChangeSetRanGenerator.java:77-80`). Djogi should declare the PK in DDL. Cheap safety.
6. **Liquibase's diff-doesn't-detect-renames.** `DiffToChangeLog` emits add+drop for renames (no identity mapping). Djogi can defer rename detection, but should not claim to detect renames from diffs — that road leads to data loss when the heuristic guesses wrong.

### Defer
1. **Context/label filtering**. Liquibase's `CONTEXTS` and `LABELS` columns (`CreateDatabaseChangeLogTableGenerator.java:63-64`) support environment-scoped deployments ("run these changesets only in production"). Useful but adds complexity. Defer until Djogi has a concrete multi-env story.
2. **Precondition DSL** (`TableExistsPrecondition`, `ColumnExistsPrecondition`, etc.). Powerful but requires a big surface area of checks-as-code. Defer to v2; support only `sql-precondition` (user-provided SELECT returning expected value) as the universal escape hatch initially.
3. **`changelog-sync-to-tag` style partial baseline**. Djogi's baseline/fake v1 can be "all-or-nothing up to current HEAD"; range-based fake is a v2 feature.
4. **DDL-in-transaction autodetect.** Djogi can start by trusting the operator's `execution_mode: no_txn` flag on the migration file; adding detection of `CREATE INDEX CONCURRENTLY` etc. to auto-set it is a v2 nicety.

### Surprises
1. **Liquibase's lock row has no ownership-based release.** `UnlockDatabaseChangeLogGenerator.java:25-29` does not include `WHERE LOCKEDBY = ?`. Any process can force-release any other process's lock. This is explicit (see `ReleaseLocksCommandStep`), but it means there's zero protection against accidental cross-cluster unlock.
2. **`DATEEXECUTED` is only second-precision on most databases** (`getDateTimeTypeString` returns `"datetime"` → maps to `TIMESTAMP` without fractional seconds on most dialects; only MSSQL gets `datetime2(3)` — see `CreateDatabaseChangeLogTableGenerator.java:76-80`). `ORDEREXECUTED` is the tiebreaker and comes from `ChangeLogHistoryService.getNextSequenceValue` (`MarkChangeSetRanGenerator.java:57`). Djogi should just use `TIMESTAMPTZ` with microsecond precision and sidestep the sequence column entirely.
3. **Checksum hashes the parsed Change DSL, not the emitted SQL.** `ChangeSet.generateCheckSum` (`ChangeSet.java:400-414`) calls `change.generateCheckSum()`, which reflects over `DatabaseChangeProperty` getters — so the SQL literally executed against Postgres is NOT what gets checksummed. Upgrading Liquibase to a newer version with an improved generator for the same Change DSL does not trigger a checksum mismatch. This is intentional but worth internalising: Djogi using raw SQL files can check the file bytes directly, which is simpler and more honest.
4. **`MARK_RAN` via preconditions is a powerful idiom**. `PreconditionContainer.FailOption.MARK_RAN` (`ChangeSet.java:792-796`) writes a ledger row as if the changeset ran, without executing its statements. Djogi's `<preConditions onFail="MARK_RAN">` equivalent is worth implementing — it's the clean way to handle "this object already exists because an ops engineer created it by hand".
5. **The lock table is created outside any user-defined transaction** (`StandardLockService.init` calls `database.commit()` explicitly). Two processes racing for first-time lock-table creation are reconciled by a dumb retry-with-random-sleep loop (`:152-164`), not by `CREATE TABLE IF NOT EXISTS`. Postgres 9.1+ supports `CREATE TABLE IF NOT EXISTS`, and Djogi should use it.
6. **`runOnChange` and `runAlways` are checksum-excluded in validation** (`ValidatingVisitor.java:127-133`). A changeset marked `runOnChange="true"` won't be flagged as a checksum mismatch — by design. Worth matching in Djogi for view/function definitions.

## Confidence
- Architecture: **high**
- State model: **high**
- Ledger DDL: **high**
- Lock DDL: **high**
- Execution / lock strategy: **high**
- Recovery / checksum / clearChecksums / changelog-sync: **high**
- Preconditions: **high**
- Partial-apply handling: **high** (confirmed absence)
- Out-of-order: **medium** (permissive default confirmed; Pro/OSS split for strict-order option unverified)
- Diff and rename: **medium** (confirmed no identity-rename in diff output path I read; did not exhaustively scan every comparator)
- Schema metadata: **high**
- Online-safe: **high** (confirmed absence)

## Open questions
1. Does Liquibase Pro add a stale-lock timeout or advisory-lock mode that OSS lacks? The OSS source has no such code, but I did not check a Pro jar.
2. Is there a transactional batching mode for multiple changesets under one commit? I saw per-changeset commits only; a changelog-wide transaction would require `ChangeLogIterator` to defer all commits, which `ChangeSet.execute` actively counteracts at `:868-870`. Might exist as an experimental flag I missed.
3. What does Liquibase do if `DATABASECHANGELOG` is written by version A (checksum V8) and version B upgrades checksum to V9 — does `upgradeChecksums` rewrite old rows silently, or does `ValidatingVisitor` tolerate mixed versions? I saw the latter (tolerance via `CheckSum.parse` reading the prefix), but I did not confirm the rewrite path triggers at what moment.
4. The `ignoreClasspathPrefix` flag in `ShouldRunChangeSetFilter.java:16,20-21,53-54` — what normalisation is it protecting against? I read the code but didn't trace the callers; affects whether out-of-order changesets are correctly matched.
5. `DbmsChangeSetFilter` with `V9` checksum excludes non-matching-dbms changes from the checksum input (`ChangeSet.java:404-406`). Does switching Postgres major versions cause `getDatabase().getShortName()` to stay stable? If not, cross-major upgrades could silently invalidate checksums. I did not verify `PostgresDatabase.getShortName()`.
6. Is there a machine-readable `unexpectedChangesets` report (rows in DB that aren't in the changelog)? `UnexpectedChangesetsCommandStep.java` exists — I did not read its semantics to compare to Djogi's planned drift-detection.
