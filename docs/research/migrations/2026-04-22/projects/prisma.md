# Prisma

## Metadata
- Clone path: `/home/tarunvir/projects/prisma-reference/`
- Commit SHA inspected: `62b44ac01aafbe101dad63abaab7da9747f62839` (`62b44ac01 chore(deps): update engines to 7.8.0-5.e96eae70cf4ade6a15d7e6064d5b0b4f7d835dd7`)
- Primary language: **This clone is TypeScript-only.** The Rust `schema-engine` lives in a sibling `prisma-engines` repository that is not vendored in this clone (`CLAUDE.md:210-230` documents the convention of checking out `prisma-engines` alongside `prisma`). The schema-engine binary is downloaded via `@prisma/engines` (referenced at `packages/migrate/src/SchemaEngineCLI.ts:379`).
- Total LOC of migration-relevant modules: **~6,205 lines** of TypeScript in `packages/migrate/src/` (source only, excluding tests/fixtures), measured via `wc -l`. The Rust engine LOC is not accessible from this clone.
- **Important caveat:** Every claim below about Rust engine internals (DDL of `_prisma_migrations`, checksum algorithm, diff step model, advisory locking, transaction framing) is inferred from the TypeScript wrapper, JSON-RPC contracts, test snapshots, and file paths referenced in test panic traces. Primary-source verification of those internals requires the `prisma-engines` clone.

## Architecture

**Split between TypeScript CLI and Rust engine.** The user-facing CLI is `packages/cli/`, but the migration logic proper is in `packages/migrate/`. The migrate package acts as a thin orchestration layer on top of an external Rust binary.

- **CLI package:** `packages/migrate/src/commands/` — one file per subcommand: `MigrateDev.ts`, `MigrateDeploy.ts`, `MigrateReset.ts`, `MigrateResolve.ts`, `MigrateStatus.ts`, `MigrateDiff.ts`, `DbPush.ts`, `DbPull.ts`, `DbExecute.ts` (`packages/migrate/src/commands/` directory listing).
- **Orchestration:** `packages/migrate/src/Migrate.ts:30-214` — the `Migrate` class coordinates fs I/O (reading the migrations directory) and delegates every non-trivial operation to an `engine: SchemaEngine` instance.
- **Two engine backends:** `SchemaEngineCLI` (spawns the Rust binary as a child process, `packages/migrate/src/SchemaEngineCLI.ts:54`) and `SchemaEngineWasm` (loads a WebAssembly build in-process, `packages/migrate/src/SchemaEngineWasm.ts:46`). Both implement the `SchemaEngine` interface at `packages/migrate/src/SchemaEngine.ts:5-129`. The CLI flow the Djogi team will care about uses `SchemaEngineCLI`.
- **Transport:** JSON-RPC 2.0 over stdin/stdout of the child process. Concrete evidence at `packages/migrate/src/SchemaEngineCLI.ts:405-416` (the `spawn(binaryPath, args, { stdio: ['pipe', 'pipe',...],... })` call) and `packages/migrate/src/SchemaEngineCLI.ts:586-597` (the `getRPCPayload` helper emitting `{ id, jsonrpc: '2.0', method, params }`). Responses are parsed in `handleResponse` at `packages/migrate/src/SchemaEngineCLI.ts:326-364`. Log lines arrive as newline-delimited JSON on stderr (`packages/migrate/src/SchemaEngineCLI.ts:474-493`), and stdout carries the JSON-RPC frames (`packages/migrate/src/SchemaEngineCLI.ts:495-502`).
- **The CLI is stateless between RPCs.** The engine child process is long-lived for the duration of one CLI invocation; messages are tagged by an incrementing `messageId` (`packages/migrate/src/SchemaEngineCLI.ts:41`), registered in `this.listeners` (`packages/migrate/src/SchemaEngineCLI.ts:59`), and resolved when the matching response arrives.
- **Exit codes from the engine:** defined in `packages/internals/src/schemaEngineCommands.ts:11-15` — `Success = 0`, `Error = 1`, `Panic = 101` (`101` being Rust's default panic exit code, which also matches `SchemaEngineExitCode.Panic` handling at `packages/migrate/src/SchemaEngineCLI.ts:459-464`).
- **Key RPC methods** (each method becomes a named JSON-RPC call): `applyMigrations`, `createDatabase`, `createMigration`, `dbExecute`, `debugPanic`, `devDiagnostic`, `diagnoseMigrationHistory`, `ensureConnectionValidity`, `evaluateDataLoss`, `getDatabaseDescription`, `getDatabaseVersion`, `introspect`, `diff` (exposed as `migrateDiff`), `markMigrationApplied`, `markMigrationRolledBack`, `reset`, `schemaPush`, `introspectSql` — see `packages/migrate/src/SchemaEngineCLI.ts:105-277` for the full method list.

**Confidence: high** on the RPC protocol and architecture split; **medium** on engine-internal details deduced from method descriptions.

## State model (source-of-truth)

- **`schema.prisma` is the canonical declarative state.** A user edits their Prisma schema files; everything downstream is derived. `packages/migrate/src/commands/MigrateDev.ts:99-105` shows schema loading via `loadSchemaContext`, and the schema context is passed into every engine call.
- **Three kinds of persisted state:**
 1. **Filesystem migrations directory** (default `prisma/migrations/`): a directory per migration containing `migration.sql` and a top-level `migration_lock.toml`. See `packages/migrate/src/utils/listMigrations.ts:20-78` for the read logic and `packages/migrate/src/utils/createMigration.ts:30-53` for the write logic.
 2. **Database ledger table** `_prisma_migrations` — inferred references at `packages/client/tests/e2e/external-tables/src/init.sql:2`, `packages/cli/src/mcp/MCP.ts:58`, `packages/cli/src/mcp/MCP.ts:88`, and `packages/migrate/src/__tests__/MigrateDev.test.ts:1067` (error `relation "_prisma_migrations" already exists`).
 3. **Schema files** in-memory as `MigrateTypes.SchemasContainer` (`packages/internals/src/migrateTypes.ts:79-81`).
- **Separation of applied state vs execution history:** partial — the ledger (see DDL below) tracks both applied migrations (identified by `migration_name` + `checksum`) and failure/rollback metadata (`finished_at`, `rolled_back_at`, `logs`, `applied_steps_count`). There is no separate audit log.
- **Shadow database** (`packages/migrate/src/SchemaEngineCLI.ts:119`, doc comment: *"This will use the shadow database on the connectors where we need one"*; also `packages/migrate/src/SchemaEngineCLI.ts:225`, *"Connection to a shadow database is only necessary when either the from or the to params is a migrations directory"*). For Postgres, it is a separately-named database created by the test harness at `packages/migrate/src/utils/setupPostgres.ts:22-23`:
 ```javascript
 await dbDefault.query(`DROP DATABASE IF EXISTS "${credentials.database}-shadowdb";`)
 await dbDefault.query(`CREATE DATABASE "${credentials.database}-shadowdb";`)
 ```
 For MySQL (`packages/migrate/src/utils/setupMysql.ts:27`) and MSSQL (`packages/migrate/src/utils/setupMSSQL.ts:37`) similarly.
- **User-configurable shadow DB URL:** `packages/config/src/PrismaConfig.ts:21-29` defines the `Datasource` shape:
 ```typescript
 const DatasourceShape = Shape.Struct({
  url: Shape.optional(Shape.String),
  shadowDatabaseUrl: Shape.optional(Shape.String),
 })
 ```
- **Shadow DB init script** (used with "external tables" feature to seed the shadow DB before the engine applies the migration history): `migrations.initShadowDb` string in `prisma.config.ts` (`packages/config/src/PrismaConfig.ts:49-69`) is flowed down through `packages/migrate/src/Migrate.ts:17` → `listMigrations(... shadowDbInitScript)` → `packages/internals/src/migrateTypes.ts:62-66`. The Rust engine reads this and runs it against the shadow DB. This is the mechanism that prevents drift detection from flagging tables the user declared as externally-managed.

**What the shadow DB is for:** replay the entire migration history against a disposable DB, then diff that against (a) the current `schema.prisma` to compute the next migration, or (b) the real DB to detect drift. This is explicit in the method doc at `packages/migrate/src/SchemaEngine.ts:21-23` (*"Note: This will use the shadow database on the connectors where we need one"*) and in the MCP server's docstring at `packages/cli/src/mcp/MCP.ts:85` (*"Reruns the existing migration history in the shadow database in order to detect schema drift"*).

**Confidence: high** on shadow DB purpose and lifecycle. **Medium** on precise isolation semantics (each engine spawn? each RPC? The test harness recreates the shadow DB per test via `DROP DATABASE IF EXISTS` + `CREATE DATABASE`, but the engine's in-RPC behaviour is not visible from TS).

## Ledger / history table

**I could not locate the exact DDL for `_prisma_migrations` in this clone.** The DDL lives in the Rust engine's per-connector code (`schema-engine/connectors/sql-schema-connector/src/flavour/{postgres,mysql,sqlite,mssql}/`), which is not present here. The TS clone only references the table by name.

What can be verified from source here:

- **Table name:** `_prisma_migrations` — observed at `packages/cli/src/mcp/MCP.ts:58`, `packages/cli/src/mcp/MCP.ts:88`, and the test panic at `packages/migrate/src/__tests__/MigrateDev.test.ts:1094`:
 ```
 db error: ERROR: relation "_prisma_migrations" already exists
   0: migration_core::state::ApplyMigrations
        at schema-engine/core/src/state.rs:199
 ```
- **Columns (from test expectations and RPC shapes):** the engine exposes these fields indirectly through the `DiagnoseMigrationHistoryOutput` and error messages:
 - `migration_name` — referenced in error P3008 *"The migration `20201231000000_draft_123` is already recorded as applied in the database"* (`packages/migrate/src/__tests__/rpc.test.ts:601`).
 - `applied_steps_count` — mentioned in the `SchemaEngine.markMigrationApplied` doc at `packages/migrate/src/SchemaEngine.ts:92-94`: *"The migration is already in the table, but in a failed state. In this case, we will mark it as rolled back, then create a new entry. [...] The started_at and finished_at will be the same."*
 - `started_at`, `finished_at` — same doc, and surfaced in the error text at `packages/migrate/src/__tests__/rpc.test.ts:531` (P3012: *"cannot be rolled back because it is not in a failed state"*).
 - `rolled_back_at` — inferred from the `markMigrationRolledBack` RPC at `packages/migrate/src/SchemaEngine.ts:99-102`: *"Mark an existing failed migration as rolled back in the migrations table. It will still be there, but ignored for all purposes except as audit trail."*
 - `logs`, `checksum` — neither column name appears in the TS code, but both are universally assumed in Prisma documentation; unverified from this clone.

- **Primary key and indexing strategy:** Not found in source; see Open questions.

- **DDL quote attempt:** The only Prisma-migration DDL I can quote verbatim from this clone is the **legacy `_Migration` table** (Prisma 1.x's schema, not the current `_prisma_migrations`), captured in a MigrateDiff test snapshot at `packages/migrate/src/__tests__/MigrateDiff.test.ts:559-571`:
 ```sql
 CREATE TABLE "_Migration" (
   "revision" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
   "name" TEXT NOT NULL,
   "datamodel" TEXT NOT NULL,
   "status" TEXT NOT NULL,
   "applied" INTEGER NOT NULL,
   "rolled_back" INTEGER NOT NULL,
   "datamodel_steps" TEXT NOT NULL,
   "database_migration" TEXT NOT NULL,
   "errors" TEXT NOT NULL,
   "started_at" DATETIME NOT NULL,
   "finished_at" DATETIME
 );
 ```
 This table is **not** the current ledger — it is the legacy lift table that Prisma 2 introspection may encounter in fixtures. The current `_prisma_migrations` DDL must be read from `prisma-engines`.

**Per-migration on-disk format** (`packages/migrate/src/__tests__/fixtures/existing-db-1-migration/prisma/migrations/20201014154943_init/migration.sql`):
```sql
-- CreateTable
CREATE TABLE "Blog" (
  "id" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,
  "viewCount20" INTEGER NOT NULL
);
```
Directory name format: `{yyyyMMddHHmmss}_{slug}` (enforced lexicographic sort at `packages/migrate/src/utils/listMigrations.ts:71`: *"Sort lexicographically by name"*).

**Migrations lockfile** (`packages/migrate/src/utils/createMigration.ts:47-53`):
```typescript
const lockfileContent = `# Please do not edit this file manually
# It should be added in your version-control system (e.g., Git)
provider = "${connectorType}"
`
```
The lockfile pins the DB provider so switching providers mid-history is caught. Filename is hardcoded `'migration_lock.toml'` at `packages/migrate/src/utils/listMigrations.ts:20`.

**Confidence: medium-low** on the ledger DDL itself (cited indirectly), **high** on the fs layout and lockfile content.

## Execution

- **Lock strategy:** **Not found in this clone.** No advisory lock, table lock, or coordination primitive appears in TypeScript. The engine is the only actor holding DB connections. Advisory locking (if any) is the Rust engine's concern.
- **Transaction boundaries:** The `applyMigrations` RPC returns `{ appliedMigrationNames: string[] }` (`packages/migrate/src/types.ts:242-244`). The return shape does not expose per-statement transaction info. Test behavior (`packages/migrate/src/__tests__/rpc.test.ts:20-45`) shows that `applyMigrations` is all-or-nothing per migration: each migration is applied and recorded in `_prisma_migrations` sequentially.
- **Partial-apply semantics:** Evidenced by `SchemaEngine.markMigrationApplied` doc at `packages/migrate/src/SchemaEngine.ts:91-95`:
 > *"There are two possible outcomes: 1) The migration is already in the table, but in a failed state. In this case, we will mark it as rolled back, then create a new entry. 2) The migration is not in the table. We will create a new entry in the migrations table. The started_at and finished_at will be the same."*

 And `markMigrationRolledBack` at `packages/migrate/src/SchemaEngine.ts:99-102`:
 > *"Mark an existing failed migration as rolled back in the migrations table. It will still be there, but ignored for all purposes except as audit trail."*

 This confirms: **failed migrations remain in the ledger** with non-null `rolled_back_at`, and subsequent attempts create fresh rows. The `applied_steps_count` field tracks progress through a multi-statement migration: when a DDL in the middle fails (e.g., `CREATE BROKEN` at `packages/migrate/src/__tests__/fixtures/existing-db-1-failed-migration/prisma/migrations/20201106130852_failed/migration.sql:2`), the engine records how many statements succeeded before the failure.
- **Concurrency posture:** single-process, single-engine. No multi-writer coordination is implemented at the TS level. The engine child process is started once per CLI invocation (`packages/migrate/src/SchemaEngineCLI.ts:405-416`). The `isRunning` flag (`packages/migrate/src/SchemaEngineCLI.ts:71`) is a process-local guard, not a distributed lock.
- **Error code P3006** (failed migration) is raised when a migration *"failed to apply cleanly to the shadow database"* (`packages/migrate/src/__tests__/MigrateDev.test.ts:594,612,636`). Error code P3008 (*"already recorded as applied"*, `packages/migrate/src/__tests__/rpc.test.ts:601`) guards against double-applying. P3012 (*"cannot be rolled back because it is not in a failed state"*, `packages/migrate/src/__tests__/rpc.test.ts:531`) guards against rolling back successful migrations.

**Confidence: medium.** The semantic contract is clear from tests and JSDoc; the actual SQL for transactional framing lives in Rust.

## Recovery

- **Checksum algorithm:** **Not found in this clone.** The `checksum` column is universally present in Prisma's `_prisma_migrations` (confirmed by documentation), but the hashing code lives in `prisma-engines`. I cannot quote the exact bytes hashed or the digest algorithm. The `downloadZip.ts` sha256 logic at `packages/fetch-engine/src/downloadZip.ts:42-47` is unrelated — it verifies the engine binary, not migration scripts. *See Open questions.*
- **`migrate resolve`:**
 - `--applied <name>` calls the `markMigrationApplied` RPC (`packages/migrate/src/commands/MigrateResolve.ts:135-140`), which per `SchemaEngine.ts:91-95` *"create[s] a new entry in the migrations table. The started_at and finished_at will be the same"* (i.e., stamps the migration as completed instantly). If the migration exists in a failed state, it first marks it as rolled back, then inserts the new entry.
 - `--rolled-back <name>` calls `markMigrationRolledBack` (`packages/migrate/src/commands/MigrateResolve.ts:164-167`), which sets `rolled_back_at` on the failed row and leaves it as audit trail.
 - These are exactly the primitives needed for (a) baselining an existing database, and (b) recovering from a mid-migration failure.
- **`migrate reset`:** `packages/migrate/src/commands/MigrateReset.ts:144-153`. Calls `migrate.reset()` then `migrate.applyMigrations()`. `SchemaEngine.reset` doc at `packages/migrate/src/SchemaEngine.ts:104-111`:
 > *"Try to make the database empty: no data and no schema. On most connectors, this is implemented by dropping and recreating the database. If that fails (most likely because of insufficient permissions), the engine attempts a 'best effort reset' by inspecting the contents of the database and dropping them individually."*

 Reset is full — there's no targeted-reset flag. Protected by an interactive prompt (`packages/migrate/src/commands/MigrateReset.ts:108-126`) and an AI-agent confirmation checkpoint (`packages/migrate/src/commands/MigrateReset.ts:128`).

- **`migrate diff`:** `packages/migrate/src/commands/MigrateDiff.ts:280-291`. Invokes the `diff` RPC with a `from` and `to` source. Each source can be: `empty`, `url` (live DB), `schemaDatamodel` (PSL files), `schemaDatasource` (PSL with datasource config), or `migrations` (a filesystem migrations dir). Types at `packages/migrate/src/types.ts:197-203`:
 ```typescript
 export type MigrateDiffTarget =
  | MigrateDiffTargetUrl
  | MigrateDiffTargetEmpty
  | MigrateDiffTargetSchemaDatamodel
  | MigrateDiffTargetSchemaDatasource
  | MigrateDiffTargetMigrations
 ```
 When one side is `migrations`, a shadow DB is used to replay the history (`packages/migrate/src/types.ts:195-196`): *"The migrations will be applied to a shadow database, and the resulting schema considered for diffing."*

 Output: either a human-readable summary or, with `--script`, an executable SQL script. Exit codes with `--exit-code`: `0` empty / `1` error / `2` non-empty diff (`packages/migrate/src/types.ts:300-308`).

 **Diff computation is AST-based, not string-based.** Strong evidence from the test snapshot at `packages/migrate/src/__tests__/rpc.test.ts:245-251` showing structured output like `[+] Added tables\n - Blog\n - _Migration` — this is a rendered tree, not a string diff. Further evidence from the structured drift output at `packages/migrate/src/__tests__/rpc.test.ts:233-252`.

- **Drift detection:** Drift is reported through the `DriftDiagnostic` discriminated union in `packages/migrate/src/types.ts:54-61`:
 ```typescript
 export type DriftDiagnostic =
  /// The current database schema does not match the schema that would be expected from applying the migration history.
  | { diagnostic: 'driftDetected'; rollback: string }
  // A migration failed to cleanly apply to a temporary database.
  | {
    diagnostic: 'migrationFailedToApply'
    error: UserFacingError
   }
 ```
 The `rollback` field is a string — a human-readable description of what's different. Actual drift output from `packages/migrate/src/__tests__/rpc.test.ts:236-248`:
 ```
 Drift detected: Your database schema is not in sync with your migration history.

 The following is a summary of the differences between the expected database schema given your migrations files, and the actual schema of the database.

 It should be understood as the set of changes to get from the expected schema to the actual schema.

 [+] Added tables
  - Blog
  - _Migration
 ```
 **How drift is computed:** (1) create or use a shadow DB; (2) apply the migration history to it; (3) diff the shadow DB against the real DB. Anything in the real DB that isn't in the shadow DB (= isn't described by the migrations on disk) is drift. Confirmed narratively at `packages/cli/src/mcp/MCP.ts:85`.

- **Baseline flow** (stamp):
 1. `db pull` to introspect the existing DB into PSL.
 2. `migrate dev --create-only` to create a migration without applying it.
 3. `migrate resolve --applied <name>` to stamp it as already-applied in the ledger.

 Captured end-to-end in `packages/migrate/src/__tests__/Baseline.test.ts:23-80`. Also documented by `MigrateStatus.ts:170-198` which detects the "no migrations table" case and suggests the baseline flow.

- **History divergence** vs **database behind** vs **migrations dir behind** (`packages/migrate/src/types.ts:63-74`):
 ```typescript
 export type HistoryDiagnostic =
  | { diagnostic: 'databaseIsBehind'; unappliedMigrationNames: string[] }
  | { diagnostic: 'migrationsDirectoryIsBehind'; unpersistedMigrationNames: string[] }
  | {
    diagnostic: 'historiesDiverge'
    lastCommonMigrationName: string | null
    unpersistedMigrationNames: string[]
    unappliedMigrationNames: string[]
   }
 ```
 These are the three canonical history states. Note `lastCommonMigrationName` in `historiesDiverge` — Prisma finds the LCA of the two histories, which is exactly what a git-style reconciliation needs.

- **Out-of-order policy:** Migrations are sorted lexicographically by directory name (`packages/migrate/src/utils/listMigrations.ts:71`). The engine expects monotonically-increasing lexicographic order (hence the `yyyyMMddHHmmss_` prefix); an applied migration appearing later in the sort order than an unapplied one is treated as divergence (`historiesDiverge` above).

**Confidence: high** on the RPC surface and drift output; **medium** on the actual shadow DB diff algorithm; **low** on checksum internals.

## Diff and generation

- **Schema introspection:** Delegated to the engine's `introspect` RPC (`packages/migrate/src/SchemaEngineCLI.ts:194-219`). Reads the live DB into PSL, which is written back to `schema.prisma`. The canonical code path is in `prisma-engines/schema-engine/sql-schema-describer`, which is not in this clone. At the TS level, `introspect` returns `{ schema: SchemasContainer, warnings: string | null, views: IntrospectionViewDefinition[] | null }` (`packages/migrate/src/types.ts:136-150`).
- **Desired schema computation:** PSL files → engine parses to DMM/datamodel → engine computes the target SQL schema → engine diffs against the current SQL schema → emits migration steps as SQL. All of this happens in Rust. At the TS boundary, the flow is: `schemaContext.schemaFiles` (filesystem) → `toSchemasContainer` (`packages/migrate/src/Migrate.ts:61-65`) → JSON-RPC `createMigration`/`diff` call → SQL script returned.
- **Rename handling:** **Heuristic-free at the engine level.** Evidence: the draft migration at `packages/migrate/src/__tests__/fixtures/existing-db-1-draft/prisma/migrations/20201203153838_draft/migration.sql:1-19`:
 ```sql
 /*
  Warnings:

  - You are about to drop the column `viewCount20` on the `Blog` table. All the data in the column will be lost.
  - Added the required column `viewCount` to the `Blog` table without a default value. This is not possible if the table is not empty.

 */
 -- RedefineTables
 PRAGMA foreign_keys=OFF;
 CREATE TABLE "new_Blog" (...);
 INSERT INTO "new_Blog" ("id") SELECT "id" FROM "Blog";
 DROP TABLE "Blog";
 ALTER TABLE "new_Blog" RENAME TO "Blog";
 PRAGMA foreign_key_check;
 PRAGMA foreign_keys=ON;
 ```
 A rename `viewCount20 → viewCount` is treated as **drop + add**, not as a rename. This is the correct behavior for Djogi: explicit descriptors-only.
- **Destructive-operation detection:** The engine returns two buckets, not one:
 - **`warnings`** (recoverable with `--accept-data-loss` / `--force`): e.g., *"You are about to drop the `Blog` table, which is not empty (1 rows)."* — produced when data loss is possible but the migration is executable. Observed at `packages/migrate/src/__tests__/rpc.test.ts:729`.
 - **`unexecutableSteps`** (hard-blocked, no `--force` override): e.g., *"Made the column `fullname` on table `Blog` required, but there are 1 existing NULL values."* — produced when the DDL cannot run at all given the current data state. Observed at `packages/migrate/src/__tests__/MigrateDev.test.ts:704`.

 Return types at `packages/migrate/src/types.ts:285-293`:
 ```typescript
 export interface EvaluateDataLossOutput {
  migrationSteps: number
  warnings: MigrationFeedback[]
  unexecutableSteps: MigrationFeedback[]
 }
 ```
 Where `MigrationFeedback = { message: string; stepIndex: number }` (`packages/migrate/src/types.ts:76-79`).

 **The classifier lives in the Rust engine** (`SqlDestructiveChangeChecker` referenced in the task brief but not locatable in this clone). At the TS level, the CLI only chooses whether to prompt/block based on which bucket the feedback is in. See `packages/migrate/src/utils/handleEvaluateDataloss.ts:6-30`:
 - If `unexecutableSteps.length > 0` and NOT `--create-only`: hard error, abort.
 - If `unexecutableSteps.length > 0` and `--create-only`: write to console.error, but continue — the user can manually edit.
 - Warnings are prompted interactively via `prompts` (`packages/migrate/src/commands/MigrateDev.ts:227-256`) or gated behind `--accept-data-loss` for `db push` (`packages/migrate/src/commands/DbPush.ts:218-247`).

- **`--create-only`:** Creates the migration directory and file but does not apply. Implemented by passing `draft: true` to `createMigration` (`packages/migrate/src/commands/MigrateDev.ts:275-287`). The engine still generates the SQL — it just doesn't execute it. This is how users can hand-edit a migration before applying.

- **Warnings are embedded as SQL comments** in the generated migration file (see the `/* Warnings:... */` block above). This is a paper trail in source control — future engineers reading git history see exactly what risks the migration carries.

**Confidence: high** on warnings-vs-unexecutable bifurcation and the rename-as-drop-add behavior; **medium** on the internal classifier.

## Schema metadata

- **Composite uniques / indexes:** Compiled from PSL `@@unique([a, b])` / `@@index([a, b])` declarations by the engine. No TS source here shows the compilation; the output appears in test snapshots like `CREATE UNIQUE INDEX "Profile.userId" ON "Profile"("userId" ASC)` (`packages/migrate/src/__tests__/MigrateDiff.test.ts:574`).
- **Introspection (aka "reflection"):** Prisma's strongest suit. The `DbPull` command (`packages/migrate/src/commands/DbPull.ts`) uses `introspect` to read existing DBs (Postgres, MySQL, SQLite, MSSQL, MongoDB, CockroachDB) and emit PSL. Test fixtures under `packages/migrate/src/__tests__/fixtures/introspection/` cover each backend with `setup.sql` files. Views are supported on Postgres (`packages/migrate/src/types.ts:140-149`).
- **External tables / enums:** First-class support for declaring parts of the DB "not mine" so the engine will not emit DDL for them (`packages/config/src/PrismaConfig.ts:85-127`). Used with `migrations.initShadowDb` to define them in the shadow DB. Experimental feature (`packages/config/src/PrismaConfig.ts:10-19`).

**Confidence: high** on the feature surface; **low** on the exact DDL mapping rules (Rust).

## Online-safe / staged migration guidance

**No first-class support in source.** Prisma does not implement multi-phase online schema change (no expand/contract, no online ALTER coordination, no pause-between-steps). The tooling it does provide:

- **`--create-only`** as an escape hatch: generate SQL, let the user edit, then apply (`packages/migrate/src/commands/MigrateDev.ts:281-287`). This is the recommended path for online-safe migrations — users hand-write the expand/contract steps.
- **`prisma db execute`** to run arbitrary SQL scripts against a datasource (`packages/migrate/src/commands/DbExecute.ts`). Intended for users who need full control outside the migration engine's diff framework.
- **`prisma migrate diff --script... | prisma db execute --stdin`** pipe pattern (`packages/migrate/src/commands/MigrateDiff.ts:100-109`) — lets users review the SQL before applying.

No warnings about long-locking DDL, no IF NOT EXISTS preflight, no documented online-DDL patterns. The engine produces single-shot DDL scripts; making them online-safe is entirely the user's responsibility.

**Confidence: high** (absence of evidence verified by grep across the migrate package).

## Rust-specific concerns

**The Rust internals are not in this clone.** I can only speak to what the TS↔Rust boundary reveals:

- **Async model:** tokio is implied by the schema-engine being a Rust service exposing JSON-RPC over stdio; confirmed narratively in `CLAUDE.md` but not verifiable here.
- **Type-safety of the migration representation:** The RPC contract is typed from the TS side (`packages/migrate/src/types.ts:83-238` defines every input/output), and from `packages/internals/src/migrateTypes.ts:2-108` for the common types. These TS types are hand-written to mirror the Rust types — they are not codegen'd from the Rust side, which means the Rust engine's internal types could be richer.
- **How the schema diff is structured:** From the RPC output, an `EvaluateDataLossOutput` has `migrationSteps: number` (just a count) plus `warnings` and `unexecutableSteps` arrays of `{ message, stepIndex }`. The actual step *data* (what SQL each step executes) is not returned — only the count and any feedback. The diff `--script` output is already-rendered SQL text. So at the Rust→TS boundary, steps are **not exposed as a typed enum**; they're flattened to SQL before crossing. Internally, the Rust engine almost certainly represents them as an enum (Prisma's publicly-known architecture has `SqlMigrationStep` as a typed variant), but that enum is not visible from this clone.
- **Connector abstraction:** The per-backend DDL lives in `schema-engine/connectors/sql-schema-connector/src/flavour/{postgres,mysql,sqlite,mssql}/` (path referenced at `CLAUDE.md` and `MigrateDev.test.ts:1069`). The TS code is connector-agnostic — it just reads `migration_lock.toml` to know the provider and passes JSON to the engine.

**Confidence: medium** on async model (reasonable inference), **low** on typed-step model (pure inference).

## Lessons for Djogi

### Adopt

- **JSON-RPC over stdio for engine↔CLI separation, if Djogi ever splits.** Clean message framing, log lines on stderr as JSON (`packages/migrate/src/SchemaEngineCLI.ts:474-493`), response matching by ID (`packages/migrate/src/SchemaEngineCLI.ts:41,322-324`). The protocol is fully specified in the engine's `json_rpc` module per the comment at `packages/migrate/src/SchemaEngineCLI.ts:97-99`. *If Djogi stays single-binary, the lesson is still valuable: keep the diff/apply engine behind a structured interface, not wired into the CLI via direct calls.*

- **The two-bucket destructive classifier: `warnings` vs `unexecutableSteps`** (`packages/migrate/src/types.ts:285-293`). This is the killer pattern. `warnings` = "you will lose data, are you sure?"; `unexecutableSteps` = "this will not run at all." The former has `--accept-data-loss` override; the latter only has `--create-only` (hand-edit-and-retry). Djogi should adopt this exact bifurcation. Concrete examples:
 - warning: dropping a non-empty table (rows > 0)
 - unexecutable: NOT NULL on a column with existing NULLs, missing default when making column required
 The classifier takes the proposed diff AND samples the current data to decide. Djogi's descriptor diff should do the same.

- **Failed migrations stay in the ledger with `rolled_back_at` as audit trail** (`packages/migrate/src/SchemaEngine.ts:91-102`). Do not silently delete failed rows. Retry creates a new row. Rolling back stamps `rolled_back_at` but leaves the row as historical evidence. This is exactly what production postmortems need.

- **`applied_steps_count` to survive mid-migration failures** (per `SchemaEngine.ts` doc comments). When a migration has 5 DDL statements and the 3rd fails, the row records `applied_steps_count=2`. The engine can resume from step 3 on retry (in theory), or the user can hand-fix step 3 and mark the whole migration applied. Djogi should track this; plain `finished_at IS NULL` is too coarse.

- **The three history-diagnostic states** (`packages/migrate/src/types.ts:63-74`): `databaseIsBehind`, `migrationsDirectoryIsBehind`, `historiesDiverge` with `lastCommonMigrationName`. These are the minimal information-preserving states for describing a dev↔prod mismatch. Djogi's `migrate status` should report exactly these (plus `failedMigrationNames` and `editedMigrationNames`).

- **`migration_lock.toml` pins the provider** (`packages/migrate/src/utils/createMigration.ts:47-53`). A two-line TOML in the migrations directory, committed to git, records `provider = "postgres"`. Prevents accidentally running SQLite-dialect migrations against a Postgres DB. Djogi should have the equivalent (even if Djogi is Postgres-only, record the connector version).

- **Rename-as-drop-add.** Prisma generates RedefineTables / DROP + CREATE + INSERT...SELECT for rename operations (`fixtures/existing-db-1-draft/prisma/migrations/20201203153838_draft/migration.sql`). No heuristic rename detection. Djogi has already decided this — Prisma validates the choice.

- **Embed warnings as SQL comments in the generated migration.** The `/* Warnings: - You are about to drop... */` block at the top of the file (`fixtures/existing-db-1-draft/prisma/migrations/20201203153838_draft/migration.sql:1-7`). Git-archaeology-friendly.

- **`--create-only` as the universal escape hatch.** When the engine can't generate a safe migration, emit the naïve one and let the user hand-edit (`packages/migrate/src/commands/MigrateDev.ts:281-287`). Djogi should have the equivalent mode for cases the typed-diff can't resolve (online-safe multi-step migrations, data backfills, etc.).

- **`migrate diff` as a first-class, read-only comparator with pipeable `--script` output** (`packages/migrate/src/commands/MigrateDiff.ts`). Accepts five source types: `empty`, `url`, `schemaDatamodel`, `schemaDatasource`, `migrations`. Exit codes: 0 / 1 / 2 with `--exit-code` (CI integration). This is the canonical way to bootstrap a new migration, verify parity, generate hotfixes, etc.

### Reject

- **Shadow database requirement.** Prisma needs a second DB to replay history for drift detection (`packages/migrate/src/utils/setupPostgres.ts:22-23`). For Djogi's target use case (Rust app using its own Postgres), a shadow DB is heavy ops burden: extra permissions, extra resource, extra URL to configure (`packages/config/src/PrismaConfig.ts:21-29`). Djogi should prefer: (a) diffing the catalog directly against descriptors, or (b) optional shadow DB only for `migrate diff --from-migrations`. Making shadow DB mandatory for drift detection is a well-known Prisma user pain point.

- **Running migrations from the CLI per invocation, no in-process integration.** Prisma's migrate is a shell tool. The engine is spawned as a child process (`packages/migrate/src/SchemaEngineCLI.ts:405-416`) and killed after each command. There is no in-process embedding for "run migrations at app startup" scenarios. Djogi wants to support `app.run_migrations().await` as a library call, so this architecture should be rejected. The Wasm variant (`SchemaEngineWasm.ts`) is closer but still opaque (`type SchemaEngineMethods = Omit<wasm.SchemaEngineWasm, 'free'>`).

- **Checksum of migration SQL as the integrity primitive.** Prisma stores a `checksum` that gets recomputed when migrations are verified (exact algorithm unverified here, see Open questions). This creates false drift alarms when a teammate reformats whitespace in a committed migration file. Djogi should think hard about whether to hash SQL text or just trust the filename (since migrations are immutable once applied — edit-after-apply is itself the bug).

- **Conflating "apply history" with "apply a single migration" in one RPC.** Prisma's `applyMigrations` applies everything pending. There is no "apply migration X only" RPC. This makes partial deployments harder. Djogi should expose both granularities.

### Defer

- **WebAssembly engine.** The Wasm path (`packages/migrate/src/SchemaEngineWasm.ts`) is marked as *"will eventually replace SchemaEngineCLI"* at `packages/migrate/src/SchemaEngineWasm.ts:41`. For Djogi-in-Rust this is a non-issue — the engine can be a normal Rust crate. Revisit only if Djogi ever grows a JS/TS client. Defer until a customer needs it.

- **Views introspection** (`packages/migrate/src/views/`, `packages/migrate/src/types.ts:140-149`). Views are Postgres-only for Prisma and a preview feature. Real-world utility is high but implementation is substantial. Revisit after core migration plumbing is stable.

- **External tables/enums** (`packages/config/src/PrismaConfig.ts:85-127`, `migrations.initShadowDb`). Mechanism for saying "Prisma, don't touch these." Useful for multi-tool-owned databases. Revisit when Djogi has a concrete user with this need.

- **Per-connector engine abstraction.** Prisma's engine handles Postgres, MySQL, SQLite, MSSQL, MongoDB, CockroachDB, Cloudflare D1. Djogi is Postgres-only. Don't build the abstraction until there's a second backend. (But design the diff types so adding one later isn't a rewrite.)

### Surprises

- **`createMigration` can return `migrationScript: null`** (`packages/migrate/src/types.ts:250-266`): *"It will be null if: 1. The migration we generate would be empty, AND 2. the `draft` param was not true"* — meaning no-op migrations are elided unless explicitly requested as drafts. The no-op-elision logic lives in the engine. Djogi should do the same — never generate empty migration files.

- **The engine panic exit code is 101, the Rust default.** (`packages/internals/src/schemaEngineCommands.ts:14`) — Rust's `panic!` exits with 101 by convention. The CLI specifically detects this and treats it as a panic (`packages/migrate/src/SchemaEngineCLI.ts:459-464`), not a normal error. Djogi should decide: panic-as-error-exit or catch-unwind-and-report.

- **The CLI prints engine-originated text by handling a special `print` RPC method** (`packages/migrate/src/SchemaEngineCLI.ts:345-362`). The engine can send `{"method": "print", "params": {"content": "..."}}` requests *back to the CLI*, which writes to stdout and sends an empty ACK. This is how the engine streams human-readable diff output. A nice trick — keeps all the rendering logic engine-side while letting the CLI own terminal I/O.

- **The legacy Prisma 1.x migration table (`_Migration`) is still encountered in fixtures and introspected** (`packages/migrate/src/__tests__/MigrateDiff.test.ts:559-571`). Legacy tables don't go away quietly. Djogi's introspection should assume it will encounter foreign migration tables.

- **AI-agent confirmation checkpoint** (`packages/migrate/src/commands/MigrateReset.ts:128`, `packages/migrate/src/utils/ai-safety.ts`). Destructive commands have a special hook to catch LLM agents that were granted shell access. This is a 2024-era addition; shows the industry moving toward "treat the AI as a hostile user" gating. Djogi should think about this.

- **Logs arrive as newline-delimited JSON on stderr** (`packages/internals/src/schemaEngineCommands.ts:20-21`) — each log line is a full JSON object `{ timestamp, level, fields: { message,... }, target }`. Errors with `is_panic: true` field are the panic-indicator.

## Confidence

| Section | Level |
|---|---|
| Architecture | high |
| State model | high |
| Ledger DDL | medium-low (DDL itself not in clone; columns inferred from RPC types and test snapshots) |
| Execution (locking, txn framing) | medium (inferred from semantics; SQL not visible) |
| Recovery (resolve, reset, diff, drift) | high on RPC behavior; low on checksum internals |
| Diff and generation | high on warnings-vs-unexecutable; medium on classifier internals |
| Schema metadata | high on surface; low on DDL mapping rules |
| Online-safe migrations | high (verified absence of feature) |
| Rust-specific concerns | medium (boundary-inference only) |

## Open questions

1. **Exact DDL of `_prisma_migrations` per connector.** In this clone the DDL is referenced but never quoted. To resolve: clone `prisma-engines` and inspect `schema-engine/connectors/sql-schema-connector/src/flavour/postgres/migration_persistence.rs` (likely path per the `CLAUDE.md` layout hint).

2. **Checksum algorithm for migration integrity.** Is it SHA-256 of the raw `migration.sql` bytes, or of a normalized representation (whitespace-collapsed, comment-stripped)? Either way, where is the compute code? The TS side only carries the value through as an opaque string. Resolve by grepping the `prisma-engines` repo for `checksum` in `schema-engine/core/` and `schema-engine/connectors/`.

3. **Advisory lock / lock table on apply.** Does the engine take a `pg_advisory_lock(key)` before running migrations on Postgres? Or a `SELECT... FOR UPDATE` on `_prisma_migrations`? Or nothing (first-writer-wins)? This matters enormously for concurrent deployment safety. Not resolvable from this clone.

4. **Transactional framing per migration.** Does `applyMigrations` wrap each migration in `BEGIN... COMMIT`, or does it apply each statement individually? How is non-transactional DDL (e.g., `CREATE INDEX CONCURRENTLY`, `ALTER TYPE... ADD VALUE` on older Postgres) handled? Inspection of `schema-engine/core/src/commands/apply_migrations.rs` needed.

5. **`SqlMigrationStep` enum structure.** The internal typed representation of a single DDL step before it becomes SQL text. This is what the task brief asked about and what Djogi wants to model. Would be in `schema-engine/connectors/sql-schema-connector/src/sql_migration.rs` or similar. Not in this clone.

6. **`SqlDestructiveChangeChecker` implementation.** What exactly qualifies as an "unexecutable step" vs a "warning"? Is the data probe per-column (`COUNT(*) WHERE col IS NULL`) or deeper? Probably in `schema-engine/connectors/sql-schema-connector/src/flavour/postgres/destructive_change_checker.rs`.

7. **Shadow DB reuse vs recreate per RPC.** Does the engine drop and recreate the shadow DB per `createMigration` / `diagnoseMigrationHistory` / `evaluateDataLoss`, or does it reuse? Implications for cost of shadow DB ops.

8. **`editedMigrationNames`** (`packages/migrate/src/types.ts:280`). How is "edited after applied" detected? Presumably by recomputing the checksum and comparing to the stored value — but that's speculative.

9. **Dev database auto-reset trigger on drift.** At `packages/migrate/src/commands/MigrateDev.ts:169-183`, `devDiagnostic.action.tag === 'reset'` causes the CLI to recommend `prisma migrate reset`. What are all the conditions on the Rust side that produce `{ tag: 'reset', reason }`? Drift is one — are there others (schema-engine can't figure out incremental diff, etc.)?

10. **Output ordering of concurrent migrations.** If two developers independently create migrations with the same lexicographic timestamp collision, which wins? The engine's behavior is not documented at the TS level. The default name format uses `yyyyMMddHHmmss` (14 chars); two developers pushing in the same second produce colliding directory names.

---

## Patch pass: prisma-engines Rust source (commit 3c6e192)

**Clone path:** `/home/tarunvir/projects/prisma-engines-reference/`
**Scope of this patch:** resolve the open questions listed above that required Rust source access. All claims below are cited to files inside the clone. The original note above is left intact; contradictions and refinements are flagged explicitly.

### 1. `_prisma_migrations` DDL — verbatim (Postgres)

**Confidence: high** — read directly from source.

`schema-engine/connectors/sql-schema-connector/src/flavour/postgres.rs:522-537`:

```sql
CREATE TABLE _prisma_migrations (
  id           VARCHAR(36) PRIMARY KEY NOT NULL,
  checksum        VARCHAR(64) NOT NULL,
  finished_at       TIMESTAMPTZ,
  migration_name     VARCHAR(255) NOT NULL,
  logs          TEXT,
  rolled_back_at     TIMESTAMPTZ,
  started_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
  applied_steps_count   INTEGER NOT NULL DEFAULT 0
);
```

Column notes:
- `id`: UUID v4 rendered as a 36-char string (`uuid::Uuid::new_v4().to_string()` at `sql_migration_persistence.rs:51`). Not a serial. No sequence.
- `checksum`: VARCHAR(64) — exactly 64 hex chars (SHA-256 = 32 bytes = 64 hex digits, enforced by `CHECKSUM_STR_LEN: usize = 64` at `schema-engine/connectors/schema-connector/src/checksum.rs:52`).
- `finished_at` / `rolled_back_at`: nullable TIMESTAMPTZ. `finished_at IS NULL AND rolled_back_at IS NULL` = currently-running or failed. `rolled_back_at IS NOT NULL` = failed and acknowledged.
- `logs`: TEXT, nullable. Written only on failure (`record_failed_step` at `sql_migration_persistence.rs:118-128` sets `logs` to the error string). On success, `logs` is never written after the initial insert (which omits it), so it stays `NULL`.
- `started_at`: set at row insertion (`record_migration_started_impl`, `sql_migration_persistence.rs:80-99`). The DB DEFAULT `now()` is never reached in practice because the app always supplies the value explicitly.
- `applied_steps_count`: INTEGER NOT NULL DEFAULT 0. Incremented by `record_successful_step` (`sql_migration_persistence.rs:101-116`) which runs `applied_steps_count = applied_steps_count + 1` per successful statement. There is no DDL index on this table beyond the primary key.

**Other connectors for comparison:**

SQLite (`flavour/sqlite.rs:194-208`):
```sql
CREATE TABLE "_prisma_migrations" (
  "id"          TEXT PRIMARY KEY NOT NULL,
  "checksum"       TEXT NOT NULL,
  "finished_at"      DATETIME,
  "migration_name"    TEXT NOT NULL,
  "logs"         TEXT,
  "rolled_back_at"    DATETIME,
  "started_at"      DATETIME NOT NULL DEFAULT current_timestamp,
  "applied_steps_count"  INTEGER UNSIGNED NOT NULL DEFAULT 0
);
```

MySQL (`flavour/mysql.rs:292-307`):
```sql
CREATE TABLE _prisma_migrations (
  id           VARCHAR(36) PRIMARY KEY NOT NULL,
  checksum        VARCHAR(64) NOT NULL,
  finished_at       DATETIME(3),
  migration_name     VARCHAR(255) NOT NULL,
  logs          TEXT,
  rolled_back_at     DATETIME(3),
  started_at       DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),
  applied_steps_count   INTEGER UNSIGNED NOT NULL DEFAULT 0
) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
```

MSSQL (`flavour/mssql.rs:238-250`):
```sql
CREATE TABLE [<schema>].[_prisma_migrations] (
  id           VARCHAR(36) PRIMARY KEY NOT NULL,
  checksum        VARCHAR(64) NOT NULL,
  finished_at       DATETIMEOFFSET,
  migration_name     NVARCHAR(250) NOT NULL,
  logs          NVARCHAR(MAX) NULL,
  rolled_back_at     DATETIMEOFFSET,
  started_at       DATETIMEOFFSET NOT NULL DEFAULT CURRENT_TIMESTAMP,
  applied_steps_count   INT NOT NULL DEFAULT 0
);
```

**Djogi note:** The Postgres DDL has no secondary indexes. The table is always small (tens to hundreds of rows), so full scans are fine. Djogi should do the same — no indexes beyond the PK needed.

### 2. Checksum algorithm

**Confidence: high** — the entire algorithm is in one 133-line file.

File: `schema-engine/connectors/schema-connector/src/checksum.rs`

```rust
fn compute_checksum(script: &str) -> [u8; 32] {
  use sha2::{Digest, Sha256};
  let mut hasher = Sha256::new();
  hasher.update(script);
  hasher.finalize().into()
}
```
(`checksum.rs:43-48`)

What is hashed: **the raw UTF-8 bytes of `migration.sql`**, including comments and whitespace, with no normalization before hashing. No trimming, no collapsing.

How it is formatted: lowercase hex, zero-padded to 64 chars (`checksum.rs:64-75`). The old format (pre-fix for issue #1887) skipped zero-padding (`checksum.rs:84-94`).

Where it is computed: `render_checksum(script)` is called at two sites:
1. `migration_persistence.rs:35` — inside `mark_migration_applied` (the baseline/stamp path), hashes the script file at stamp time.
2. `migration_persistence.rs:64` — inside `record_migration_started`, hashes the script at the start of each real apply. The checksum is written to the DB row before execution, so if the process dies mid-migration, the checksum is still stored.

Where it is compared: `migrations_directory.rs:97-99`, method `matches_checksum` — reads the current on-disk script, calls `script_matches_checksum(script, checksum_str)`. That function tries the checksum against three variants of the script (`\r\n` normalized to `\n`, `\n` normalized to `\r\n`, and the original) to survive git line-ending transformations (`checksum.rs:21-23`).

The test at `checksum.rs:102-107` shows the concrete value:
```
render_checksum("hello") == "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
```
(That is SHA-256 of `"hello"` in lowercase hex — verifiable externally.)

**Djogi implication:** Whitespace-only edits to a committed migration will flip its checksum and trigger `editedMigrationNames`. There is no "normalize whitespace before hashing" option. The line-ending tolerance is the only normalization applied. This confirms the existing note's tentative recommendation to think hard about whether to hash SQL text.

### 3. Two-bucket destructive classifier — Rust-side rules

**Confidence: high** — rules are explicit enum variants.

**`UnexecutableStepCheck` enum** (`sql_destructive_change_checker/unexecutable_step_check.rs:7-13`):
```rust
pub(crate) enum UnexecutableStepCheck {
  AddedRequiredFieldToTable(Column),
  AddedRequiredFieldToTableWithPrismaLevelDefault(Column),
  MadeOptionalFieldRequired(Column),
  MadeScalarFieldIntoArrayField(Column),
  DropAndRecreateRequiredColumn(Column),
}
```

Rules for each unexecutable variant (from `evaluate()` at `unexecutable_step_check.rs:36-139`):
- `AddedRequiredFieldToTable`: fires if table row count > 0 (probes `COUNT(*)`). If row count = 0, returns `None` — safe.
- `AddedRequiredFieldToTableWithPrismaLevelDefault`: same — row count > 0 is unexecutable; asks user to add as optional first, populate, then make required.
- `MadeOptionalFieldRequired`: probes `COUNT(*)` AND `COUNT(*) WHERE col IS NOT NULL`. Fires only if null count > 0. If all non-null, returns `None` — safe.
- `MadeScalarFieldIntoArrayField`: probes `COUNT(*) WHERE col IS NOT NULL`. Fires if non-null values exist.
- `DropAndRecreateRequiredColumn`: fires if row count > 0. Catches type-change-requires-drop-recreate on a required column.

**`SqlMigrationWarningCheck` enum** (`sql_destructive_change_checker/warning_check.rs:7-48`):
```rust
pub(crate) enum SqlMigrationWarningCheck {
  DropAndRecreateColumn { table, namespace, column },
  NonEmptyColumnDrop { table, namespace, column },
  NonEmptyTableDrop { table, namespace },
  RiskyCast { table, namespace, column, previous_type, next_type },
  NotCastable { table, namespace, column, previous_type, next_type },
  PrimaryKeyChange { table, namespace },
  UniqueConstraintAddition { table, columns },
  EnumValueRemoval { enm, values },
}
```

Warning rules (from `evaluate()` at `warning_check.rs:98-214`):
- `NonEmptyTableDrop`: fires if row count > 0. Returns `None` (= no warning, safe to proceed) if count = 0.
- `NonEmptyColumnDrop`: fires if non-null value count > 0 in column. `None` if table empty or column all-null.
- `RiskyCast`: fires if non-null values exist in the column being cast. `None` if empty or all-null.
- `NotCastable`: same trigger as `RiskyCast` but the cast is not supported — warning text differs.
- `DropAndRecreateColumn`: fires if either row count or non-null value count > 0.
- `PrimaryKeyChange`: fires if row count > 0 (partial failure could leave table without PK).
- `UniqueConstraintAddition`: always fires (cannot check for duplicates without running the migration).
- `EnumValueRemoval`: always fires (no data probe — removing an enum value may fail if in use).

The Postgres-specific checker (`postgres/destructive_change_checker.rs:40-182`) makes the routing decisions: `MadeOptionalFieldRequired` → `UnexecutableStepCheck`; `RiskyCast` → `SqlMigrationWarningCheck`; `NotCastable` → `SqlMigrationWarningCheck`.

**Key non-obvious invariants:**
1. Data probes (`COUNT(*)`) run against the **production database** at `evaluateDataLoss` time, not against the shadow DB. This means the classifier sees real production data.
2. `UniqueConstraintAddition` and `EnumValueRemoval` always produce a warning regardless of data — there is no data probe for these two.
3. `DropAndRecreateRequiredColumn` is unexecutable, but `DropAndRecreateColumn` (nullable version) is just a warning. The required-ness of the column determines which bucket.

### 4. Advisory lock / concurrency

**Confidence: high** — SQL is verbatim in source.

Postgres (`flavour/postgres.rs:363-389`):
```sql
SELECT pg_advisory_lock(72707369)
```
The key `72707369` is a hardcoded integer the team chose to identify Prisma Migrate. The comment at `postgres.rs:374` says: *"It does not have any meaning, but it should not be used by any other tool."* The lock is session-scoped (held until the connection closes). Timeout: `ADVISORY_LOCK_TIMEOUT = Duration::from_secs(10)` (`postgres.rs:38`). On timeout, the engine surfaces a `DatabaseTimeout` user-facing error with the SQL and timeout in ms (`postgres.rs:381-383`).

CockroachDB **does not** take an advisory lock — explicit fallthrough at `postgres.rs:364-368` with the comment linking to the CockroachDB issue tracker (cockroachdb/cockroach#13546).

MySQL (`flavour/mysql.rs:159-176`):
```sql
SELECT GET_LOCK('prisma_migrate', 10)
```
Key is the string `'prisma_migrate'`, timeout is 10 seconds. PlanetScale is exempt (advisory locking times out at 20s on PlanetScale and the branching model provides isolation; `mysql.rs:161-170`).

MSSQL (`flavour/mssql.rs:178-186`):
```sql
sp_getapplock @Resource = 'prisma_migrate', @LockMode = 'Exclusive', @LockOwner = 'Session'
```
No explicit timeout — uses the server-configured default (`mssql.rs:181`).

SQLite: uses `with_connection` which wraps a file-level exclusive lock through Quaint; no separate advisory lock call visible.

`acquire_lock()` is called at the top of `apply_migrations` (`commands/src/commands/apply_migrations.rs:20`) and `mark_migration_applied` (`commands/src/commands/mark_migration_applied.rs:15`) — the two mutating commands. It is **not** called by `diagnose_migration_history` or `evaluateDataLoss` (read-only paths).

**Djogi implication:** Prisma explicitly uses `pg_advisory_lock(72707369)` — Djogi must pick a different key if it also uses advisory locks, or document the key so operators know both systems cannot safely coexist on the same database without explicit coordination.

### 5. Transaction boundaries

**Confidence: high** on Postgres statement-splitting; **high** on ledger update pattern.

Postgres `apply_migration_script` (`flavour/postgres/connector/native/mod.rs:146-156`):
```rust
// We split the script into statements rather than submitting it all at once.
// The reason is that the Postgres simple protocol automatically wraps the script
// in a transaction, which is sometimes undesirable (e.g. when the script contains
// statements that cannot be run inside a transaction like `CREATE INDEX CONCURRENTLY`).
for stmt in split_script_into_statements(script) {
  match client.simple_query(stmt).await {... }
}
```

So: **each statement in the migration script runs in its own auto-commit transaction** (using the Postgres simple protocol). There is no explicit `BEGIN`/`COMMIT` around the whole migration script. This is a deliberate design choice to allow `CREATE INDEX CONCURRENTLY` and similar statements that cannot run inside an explicit transaction.

**Partial-apply representation in the ledger:**
1. `record_migration_started` inserts the row with `started_at`, `checksum`, `applied_steps_count=0`, `finished_at=NULL`, `logs=NULL`.
2. Each successful statement calls `record_successful_step` → `applied_steps_count += 1`.
3. On failure: `record_failed_step` sets `logs` to the error string. `finished_at` remains NULL; `rolled_back_at` remains NULL.
4. A failed row is identified by `finished_at IS NULL AND rolled_back_at IS NULL` (`apply_migrations.rs:113-114`, `detect_failed_migrations`).
5. On the next `applyMigrations` run, `detect_failed_migrations` finds this row and raises `FoundFailedMigrations` — blocking all further applies until the user resolves via `migrate resolve`.

So: **no automatic retry or resume.** The `applied_steps_count` column records how far the migration got, but the engine does not resume from that step on retry. It is audit information, not a resume pointer. This is the original note's "theory" confirmed and corrected.

**The CockroachDB path is different:** for shadow DB replay only (not for real applies), CockroachDB wraps all migrations in a `BEGIN`/`COMMIT` block (`postgres.rs:683-716`) to work around CockroachDB's slow DDL outside transactions.

### 6. Shadow database mechanics

**Confidence: high** — lifecycle is explicit in source.

Shadow DB creation path (Postgres, no external shadow DB configured):
`flavour/postgres/connector/native/shadow_db.rs:53-113`:

1. `new_shadow_database_name()` generates a UUID-based name: `format!("prisma_migrate_shadow_db_{}", uuid::Uuid::new_v4())` (`lib.rs:639-641`).
2. `CREATE DATABASE "{shadow_database_name}"` runs on the main connection (`shadow_db.rs:56-60`).
3. A new `PostgresConnector` connects to the shadow DB, validates the connection (`shadow_db.rs:73-75`).
4. The migration history is replayed: for each migration in `applied_migrations`, calls `apply_migration_script` on the shadow connection (`shadow_db.rs:82-93`, inner loop at `postgres.rs:698-710`).
5. `describe_schema` introspects the shadow DB to get its SQL schema.
6. **Shadow DB is always dropped** — `dispose()` then `DROP DATABASE IF EXISTS "{name}" WITH (FORCE)` (Postgres ≥13) or `DROP DATABASE IF EXISTS "{name}"` fallback (`shadow_db.rs:95-112`, `drop_db_try_force` at `shadow_db.rs:177-192`).

The shadow DB is **created and destroyed per RPC call** that needs schema-from-migrations. It is not cached across calls. The `MigrationSchemaCache` in `commands/src/migration_schema_cache.rs` is an in-process cache keyed on the `DefaultHasher` hash of the migration directories list — it caches the *result* (the SqlSchema) so that repeated diagnose calls within one engine invocation avoid recreating the shadow DB, but the cache is process-lifetime, not cross-invocation.

External shadow DB path (user provided `shadowDatabaseUrl`): instead of creating a new DB, the engine connects to the external one, calls `reset()` (drop all objects), replays migrations, introspects, and does **not** drop the external DB at the end (`shadow_db.rs:117-143`).

### 7. Rename handling — Rust-side confirmation

**Confidence: high.**

The `SqlMigrationStep` enum (`sql_migration.rs:481-516`) has no `RenameColumn` variant. The complete set of table-modifying steps is:
- `AlterTable` (with `TableChange::AddColumn`, `TableChange::DropColumn`, `TableChange::AlterColumn`)
- `RedefineTables` (drop-and-recreate the table using INSERT...SELECT for data migration)
- `CreateTable` / `DropTable`

There is a `RenameIndex` and `RenameForeignKey` step, but **no `RenameColumn`**. A column rename with `@map` (the PSL `@@map` / `@map` annotation for field-to-column name mapping) is handled purely at the PSL layer: the column's DB name is changed in the schema, so the differ sees a column with the old name disappeared and a column with the new name appeared — which is `DropColumn` + `AddColumn` under `AlterTable`. The data in the renamed column is lost unless the user hand-edits the generated migration to use `ALTER TABLE... RENAME COLUMN`.

This confirms the original note's "heuristic-free" claim and Djogi's `#[field(renamed_from = "old_name")]` approach — but note that Djogi's annotation explicitly handles the rename at the differ level, which Prisma does **not** do. That is a Djogi advantage.

### 8. Baseline / stamp semantics

**Confidence: high.**

`commands/src/commands/mark_migration_applied.rs`:

1. Lock acquired (`acquire_lock().await?` at line 15).
2. The target migration is found in the filesystem migrations list (not the DB). If not found on disk, raises `MigrationToMarkAppliedNotFound` (line 23-27).
3. The script is read from disk (line 29-33).
4. Any existing rows for this migration name that are in a failed state (`finished_at IS None AND rolled_back_at IS None`) are marked rolled back (lines 56-65).
5. `mark_migration_applied(migration_name, &script)` is called (line 69), which computes `render_checksum(script)` and inserts a row with `started_at = now()`, `finished_at = now()` (same timestamp), `applied_steps_count = 0`, `logs = ""` (`sql_migration_persistence.rs:45-66`).

**The baseline row is distinguishable from a normally-applied row by `applied_steps_count = 0` and `logs = ""`** — but `finished_at IS NOT NULL` makes it appear "successful" to all future queries. There is no explicit "was this row stamped or actually applied?" flag.

If the migrations table does not yet exist, `baseline_initialize()` is called first (line 41-43), which creates `_prisma_migrations` without checking if the schema is empty — the normal `initialize()` path would refuse to create the table if the schema is non-empty (`sql_migration_persistence.rs:33-38`), but `baseline_initialize` skips that check (`sql_migration_persistence.rs:10-14`).

### 9. Out-of-order policy

**Confidence: high.**

`apply_migrations.rs:36-45` (the unapplied-migration filter):
```rust
let unapplied_migrations: Vec<&MigrationDirectory> = migrations_from_filesystem
 .migration_directories
 .iter()
 .filter(|fs_migration| {
    !migrations_from_database
     .iter()
     .filter(|db_migration| db_migration.rolled_back_at.is_none())
     .any(|db_migration| db_migration.migration_name == fs_migration.migration_name())
  })
 .collect();
```

The filter simply checks: "is this filesystem migration absent from the non-rolled-back DB rows?" It does not check ordering. A migration that is on-disk but not in the DB will be applied regardless of its lexicographic position relative to already-applied migrations.

However, the *diagnose* path (`diagnose_migration_history.rs:92-123`) detects the ordering anomaly and classifies it as `HistoriesDiverge` (both `fs_migrations_not_in_db` and `db_migrations_not_in_fs` non-empty). The `diagnose` output surfaces this to `MigrateDev`, which **does not apply** in a diverged-history state — it asks the user to reset or resolve.

For `migrate deploy` (production apply), there is no pre-flight diagnose call. The `applyMigrations` command will apply any unapplied filesystem migration in filesystem order, regardless of whether it is dated earlier than already-applied migrations. **Out-of-order apply is possible and silent in `migrate deploy`.**

This is a significant finding: `migrate deploy` does not enforce the "no out-of-order" invariant. `migrate dev` enforces it indirectly via `devDiagnostic` → divergence → require reset.

### 10. Drift detection — introspection pipeline

**Confidence: high** on algorithm; **medium** on internals of `schema_from_database`.

`diagnose_migration_history.rs:126-192` (the drift detection block):

1. **Compute expected schema:** apply only the migrations that are `finished_at IS NOT NULL AND rolled_back_at IS NULL` (successfully-applied ones) to the shadow DB. The result is `from: SqlSchema`.
2. **Get actual schema:** `connector.schema_from_database(namespaces)` — introspects the real production DB.
3. **Diff:** `dialect.diff(from, to, &filter)` — produces an internal `SqlMigration` diff object.
4. **Check emptiness:** `dialect.migration_is_empty(&mig)` — if the diff is empty, no drift. If non-empty, emit `DriftDiagnostic::DriftDetected { summary: dialect.migration_summary(&drift) }`.

The `from` schema (shadow DB replay) is cached in-process via `MigrationSchemaCache` (`migration_schema_cache.rs`) using `DefaultHasher` on the migration directory names. The cache prevents re-creating the shadow DB on every diagnosis within a single engine process.

Drift detection is **opt-in** (`input.opt_in_to_shadow_database` at `diagnose_migration_history.rs:143`). If the flag is false, drift is not checked — only history alignment (missing/extra migrations, failed rows, checksum mismatches) is reported. This means the `devDiagnostic` command can request drift detection while `migrate status` may skip it.

The `edited_migration_names` list (`diagnose_migration_history.rs:99-107`) is computed by calling `fs_migration.matches_checksum(&db_migration.checksum)` — reading the on-disk file, computing its SHA-256, and comparing to the stored checksum. This is the mechanism for detecting post-apply edits.

### Patch-pass surprises vs existing Prisma note

1. **`logs` column written only on failure, not on success.** The original note correctly identified `logs` as a column but did not know whether it was always populated. Source confirms: `record_failed_step` sets `logs` to the error string; on success, `logs` is never set after the initial insert (which omits it, so it stays NULL). The `mark_migration_applied` (stamp) path writes `logs = ""` — an empty string, not NULL. This means `logs IS NULL` is not a reliable "no error" signal for stamped rows.

2. **`applied_steps_count` is not a resume pointer.** The original note speculated it might allow resuming from a partially-applied migration. It does not. The engine always re-applies the whole migration from scratch; `applied_steps_count` is only audit evidence of how far it got before failing. Retrying a failed migration with `migrate resolve --applied` stamps it without re-executing any steps.

3. **No per-migration transaction on Postgres.** The original note stated "The return shape does not expose per-statement transaction info." The Rust source confirms there is no transaction: each statement runs as auto-commit via `simple_query`. A multi-statement migration can partially apply — statements 1-3 committed, statement 4 fails — with no rollback of the prior statements. The `applied_steps_count` records 3 in this case. This is a significant production risk that the original note did not have visibility into.

4. **Advisory lock key is public and hardcoded.** `72707369` is the key. Any other tool that happens to acquire `pg_advisory_lock(72707369)` will deadlock with Prisma Migrate. Djogi must use a different key.

5. **Out-of-order apply is silent in `migrate deploy`.** The original note described `historiesDiverge` correctly but did not trace how `applyMigrations` handles it. Source shows `applyMigrations` applies any unapplied filesystem migration without ordering checks — it relies on the caller (`MigrateDev`) to pre-flight with `devDiagnostic`. `migrate deploy` skips the pre-flight.

6. **Shadow DB name is UUID-based, not a predictable suffix.** The TS test harness used `{database}-shadowdb` as the name. The Rust engine uses `prisma_migrate_shadow_db_{uuid}`. These are different conventions. The TS tests create the shadow DB externally; the engine creates it internally with its own naming. This means the TS test fixture at `setupPostgres.ts:22-23` (cited in the original note) is test infrastructure, not production behavior.

7. **`applied_steps_count = 0` and `logs = ""` distinguish a stamped row** from a normally-applied row (which would have `applied_steps_count > 0` if it ran any statements). But there is no explicit `was_stamped` boolean column. This is a subtle invariant — `applied_steps_count = 0` with `finished_at IS NOT NULL` means the migration was stamped, not that it had zero statements.

### Patch-pass lessons for Djogi

**Adopt:**

- **SHA-256 of raw script bytes, lowercase 64-char hex, with line-ending normalization.** `schema-engine/connectors/schema-connector/src/checksum.rs:43-48`. Store 64-char VARCHAR (or TEXT). Try three variants (`\r\n`→`\n`, `\n`→`\r\n`, original) on comparison. This is tested and battle-hardened.

- **Advisory lock with a documented key.** `postgres.rs:363-389`: `SELECT pg_advisory_lock(72707369)` with a 10-second timeout surfaced as a user-facing error. Djogi should do the same — pick a key (not 72707369), document it, and raise a clear timeout error. Lock must be taken before all writes to the ledger.

- **Ledger row lifecycle: INSERT on start, UPDATE on step success, UPDATE on failure, UPDATE on finish.** `sql_migration_persistence.rs:80-139`. Four explicit transitions, four explicit SQL operations. Failed rows are identified by `finished_at IS NULL AND rolled_back_at IS NULL`. This is the minimal correct state machine.

- **`applied_steps_count` as audit evidence, not a resume pointer.** `migration_persistence.rs:80-86` (trait doc). Increment per successful statement. Do not try to resume from this count. It exists so postmortems can see how far a migration got.

- **Statement-level splitting for Postgres, not script-level.** `postgres/connector/native/mod.rs:150-157`. Submit each statement via simple_query (auto-commit) so `CREATE INDEX CONCURRENTLY` and similar non-transactional DDL can run. Accept that partial apply is possible — the ledger state machine handles it.

- **Two history-check modes: read-only diagnose vs mutating apply.** `diagnose_migration_history.rs` vs `apply_migrations.rs`. The diagnose path takes no lock and writes nothing. The apply path acquires the advisory lock first. Djogi should enforce this separation.

- **Detect `edited_migration_names` by re-checksumming on-disk files against stored checksums.** `diagnose_migration_history.rs:99-107`. Read the file, SHA-256 it, compare to DB. No other mechanism.

**Reject:**

- **No per-migration transaction wrapping on Postgres.** Prisma's choice (`postgres/connector/native/mod.rs:152-155`) is deliberate — allows `CREATE INDEX CONCURRENTLY`. But Djogi targets Postgres 18+ where transactional DDL is supported for most operations. Djogi should evaluate whether to wrap migrations in `BEGIN`/`COMMIT` by default and carve out only the explicitly-non-transactional statements (or document that migrations should be single-statement when non-transactional DDL is needed).

- **Out-of-order silent apply in the deploy path.** `apply_migrations.rs:36-45`. Prisma allows out-of-order application in `migrate deploy`. Djogi's spec is more strict: either reject or warn. Source confirms Prisma's behavior is a deliberate loose policy, not a tested invariant. Djogi should enforce ordering.

- **UUID v4 as the ledger row PK.** `sql_migration_persistence.rs:51`. Prisma uses random UUIDs, which have poor btree locality. Since Djogi uses HeerId already and the ledger table is tiny, using HeerId (BIGINT, time-ordered) as the row PK is strictly better. The current-ledger PK being a random string also means `applied_steps_count` / `record_successful_step` must look up by `id` (a WHERE on a PRIMARY KEY scan) — fine for a tiny table but worth noting.

**Defer:**

- **In-process `MigrationSchemaCache` for shadow DB result reuse.** `commands/src/migration_schema_cache.rs`. Prisma caches the `SqlSchema` result of replaying migrations to avoid re-creating the shadow DB on repeated diagnose calls within one engine invocation. Djogi should defer this optimization until shadow DB overhead is actually measured.

- **CockroachDB transaction-wrapped shadow replay.** `postgres.rs:683-716`. The `BEGIN`/`COMMIT` wrap for shadow replay on CockroachDB. Djogi is Postgres-only; defer until/unless CockroachDB support is added.

**Clarify in Djogi spec:**

- **What key to use for `pg_advisory_lock`.** Djogi's spec mentions advisory locks but does not specify the key. The key must be documented, must not be `72707369` (Prisma's key), and must not conflict with other tools the user might run alongside Djogi.

- **Whether Djogi's migrate-apply wraps each migration in a transaction.** The current spec (`docs/spec/migrations.md`) should explicitly say whether per-migration transactions are used on Postgres. Given Djogi targets Postgres 18+ exclusively and `CREATE INDEX CONCURRENTLY` cannot run in a transaction, the recommended Djogi pattern for online-safe indexes should be documented as a separate migration (or a `--no-transaction` flag per migration file).

- **`applied_steps_count` semantics vs resume semantics.** The spec should state clearly that `applied_steps_count` is audit evidence only — resuming from step N is not supported without a manual `migrate resolve`. If resume-from-step is desired, it must be an explicit design choice with its own implementation.

- **Out-of-order policy in `djogi migrate apply`.** The spec should say whether applying a migration that is dated before the latest applied migration is (a) an error, (b) a warning, or (c) silently applied. Prisma silently applies in deploy mode; Djogi should make an explicit choice and test it.
