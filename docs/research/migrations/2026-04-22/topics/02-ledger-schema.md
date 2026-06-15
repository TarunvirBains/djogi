# Topic 02: Ledger Schema

## Executive summary

Every migration system studied maintains at least a "which migrations were applied" record in the database. Beyond that minimum, the designs split sharply. The small end is Alembic's single-column `alembic_version` (one row per active head, not a history log at all) and Diesel/SeaORM/cot's two-column tables (version/name + applied timestamp). The large end is Flyway's `flyway_schema_history` (ten columns, success flag, execution time, a secondary index) and Liquibase's `DATABASECHANGELOG` (fourteen columns, no primary key, a separate lock table). Prisma's `_prisma_migrations` (eight columns, nullable failure timestamps, applied-steps progress counter) sits in the middle but is the only system with a multi-statement progress counter for partial-apply detection.

Of the eleven systems studied, only three store a checksum: Flyway (CRC-32 stored as signed `INTEGER`), Liquibase (MD5 with version prefix stored as `VARCHAR(35)`), and refinery (SipHash-1-3 stored as decimal `VARCHAR(255)`). Django, Alembic, Diesel, SeaORM, sea-query (no runner), refinery (yes, one), cot, and Prisma (checksum column exists, algorithm in Rust engine) vary. Django and Alembic have no checksum at all. Sea-query has no ledger.

The Djogi proposed schema is the widest of all systems surveyed. It introduces columns that no prior system has — most notably `up_checksum`/`down_checksum`/`source_checksum` as distinct fields, and `execution_mode` as a first-class column. It also repeats columns that every serious system has (`applied_at`, `applied_by`, `execution_time_ms`, `status`). The analysis below validates each column against prior art, identifies one column to drop, and identifies one column that warrants careful implementation.

Ledger / lock conflation: Flyway uses `InsertRowLock` (a sentinel row in `flyway_schema_history` with `installed_rank = -100`) as its non-advisory-lock fallback, but Postgres uses a separate advisory lock mechanism so the tables are not physically merged for Postgres. Liquibase maintains a fully separate `DATABASECHANGELOGLOCK` table. All others have no lock mechanism.

Auto-create behavior: Diesel, SeaORM, cot, and refinery all use `CREATE TABLE IF NOT EXISTS` so the ledger is silently created on first run. Django creates the table by running its ORM `schema_editor.create_model()` at startup. Flyway and Liquibase create the table during `baseline` or on first `migrate` run, respectively, via their own DDL. Alembic creates the table in the `--sql` output when starting from `base`. Prisma's Rust engine calls `create_migrations_table` at the start of `applyMigrations`. No system requires a separate `init` command purely to create the ledger; `baseline` in Flyway and `changelog-sync` in Liquibase create it if absent. The Djogi runner should follow the majority pattern: `CREATE TABLE IF NOT EXISTS` at startup, no separate init step.

---

## Comparison matrix

The table below uses the following encoding:
- **Column name** — the actual column name used
- `—` — column absent
- `(inferred)` — column not verbatim from DDL but confirmed from surrounding source
- **Djogi (proposed)** — the Djogi spec columns as understood from CLAUDE.md

| Column purpose | Django | Alembic | SQLAlch | Flyway | Liquibase | Prisma | Diesel | SeaORM | SeaQuery | refinery | cot | Djogi (proposed) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **Surrogate PK (auto-int)** | `id` serial | — | — | `installed_rank` INT | — (no PK!) | `id` VARCHAR(36) UUID | — | — | N/A | — | `id` Auto\<i32\> | — (version is PK?) |
| **Natural version string** | — | `version_num` VARCHAR(32) | N/A | `version` VARCHAR(50) nullable | `ID`+`AUTHOR`+`FILENAME` composite | — | `version` VARCHAR(50) PK | `version` String PK | N/A | `version` int4 PK | — | `version` |
| **Migration name / description** | `name` VARCHAR(255) | — | N/A | `description` VARCHAR(200) | `DESCRIPTION` VARCHAR(255) | `migration_name` VARCHAR(255) | — | — | N/A | `name` VARCHAR(255) | `name` String | `description` |
| **App / author / filename** | `app` VARCHAR(255) | — | N/A | `script` VARCHAR(1000) | `ID`+`AUTHOR`+`FILENAME` (each VARCHAR(255)) | — | — | — | N/A | `app` String | `app` String | — |
| **Checksum** | — | — | N/A | `checksum` INTEGER nullable | `MD5SUM` VARCHAR(35) | `checksum` VARCHAR(64) | — | — | N/A | `checksum` VARCHAR(255) | — | `up_checksum`, `down_checksum`, `source_checksum` |
| **Applied / exec timestamp** | `applied` TIMESTAMPTZ | — | N/A | `installed_on` TIMESTAMP DEFAULT now() | `DATEEXECUTED` TIMESTAMP | `started_at` TIMESTAMPTZ DEFAULT now() | `run_on` TIMESTAMP DEFAULT CURRENT_TIMESTAMP | `applied_at` i64 (unix seconds) | N/A | `applied_on` VARCHAR(255) RFC3339 | `applied` TIMESTAMPTZ | `applied_at` |
| **Finish timestamp** | — | — | N/A | — | — | `finished_at` TIMESTAMPTZ nullable | — | — | N/A | — | — | — |
| **Rollback timestamp** | — | — | N/A | — | — | `rolled_back_at` TIMESTAMPTZ nullable | — | — | N/A | — | — | — |
| **Execution duration** | — | — | N/A | `execution_time` INTEGER (ms) | — | — | — | — | N/A | — | — | `execution_time_ms` |
| **Success / status flag** | — (presence = success) | — | N/A | `success` BOOLEAN | `EXECTYPE` VARCHAR(10) | — (nullability of finish timestamps) | — | — | N/A | — | — | `status` |
| **Partial-apply progress** | — | — | N/A | — | — | `applied_steps_count` INTEGER DEFAULT 0 | — | — | N/A | — | — | `partial_apply_state` |
| **Out-of-order flag** | — | — | N/A | (state computed at query time) | — | — | — | — | N/A | — | — | `out_of_order_flag` |
| **Execution mode** | — | — | N/A | — | `runInTransaction` (not stored) | — | — | — | N/A | — | — | `execution_mode` |
| **Installed-by / user** | — | — | N/A | `installed_by` VARCHAR(100) | — | — | — | — | N/A | — | — | `applied_by` |
| **Migration type** | — | — | N/A | `type` VARCHAR(20) | `EXECTYPE` VARCHAR(10) | — | — | — | N/A | — | — | — |
| **Deployment group ID** | — | — | N/A | — | `DEPLOYMENT_ID` VARCHAR(10) | — | — | — | N/A | — | — | — |
| **Logs / error text** | — | — | N/A | — | `COMMENTS` VARCHAR(255) | `logs` TEXT | — | — | N/A | — | — | — |
| **Ordering sequence** | — | — | N/A | `installed_rank` (also PK) | `ORDEREXECUTED` INT | — | — | — | N/A | — | — | — |
| **Tags / labels** | — | — | N/A | — | `TAG`, `CONTEXTS`, `LABELS` VARCHAR(255) | — | — | — | N/A | — | — | — |
| **Tool version** | — | — | N/A | — | `LIQUIBASE` VARCHAR(20) | — | — | — | N/A | — | — | — |

Notes:
- SQLAlchemy does not have a migration runner; it provides schema metadata primitives consumed by Alembic. Its row is N/A throughout.
- sea-query is a DDL builder with no runner and no ledger. Its row is N/A throughout.
- Prisma's `id` column is `VARCHAR(36)` — a UUID string (not integer), used as PK. This is unique among surveyed systems.

---

## Verbatim DDL per system

### Django's `django_migrations`

Django does not store raw DDL; the table is created through its ORM's `schema_editor.create_model()`. The authoritative model (source: `projects/django.md`, from `django/db/migrations/recorder.py:32-46`):

```python
class Migration(models.Model):
  app = models.CharField(max_length=255)
  name = models.CharField(max_length=255)
  applied = models.DateTimeField(default=now)

  class Meta:
    apps = Apps()
    app_label = "migrations"
    db_table = "django_migrations"
```

Reconstructed PostgreSQL DDL (from `projects/django.md` — confidence: high):

```sql
CREATE TABLE "django_migrations" (
  "id"   serial PRIMARY KEY,
  "app"   varchar(255) NOT NULL,
  "name"  varchar(255) NOT NULL,
  "applied" timestamp with time zone NOT NULL
);
```

There is no `UNIQUE(app, name)` constraint declared in the source — Django relies on application logic. No indexes beyond the primary key. Source: `projects/django.md` (from `recorder.py:32-46`).

### Alembic's `alembic_version`

The table is defined via SQLAlchemy's schema API. Verbatim Python source (from `projects/alembic.md`, from `alembic/ddl/impl.py:151-183`):

```python
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
```

Equivalent PostgreSQL DDL (from `projects/alembic.md` — confidence: high):

```sql
CREATE TABLE alembic_version (
  version_num VARCHAR(32) NOT NULL,
  CONSTRAINT alembic_version_pkc PRIMARY KEY (version_num)
);
```

This is a current-state pointer, not a history log. One row per active branch head. No history, no checksum, no timestamp. The PK constraint was added later and is opt-out (`version_table_pk=False` removes it). Source: `projects/alembic.md` (from `alembic/ddl/impl.py:151-183`, `runtime/environment.py:540-544`).

### Flyway's `flyway_schema_history`

Verbatim Java source that generates the DDL (from `projects/flyway.md`, from `flyway-database/flyway-database-postgresql/src/main/java/org/flywaydb/database/postgresql/PostgreSQLDatabase.java:56-76`):

```java
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
    "ALTER TABLE " + table + " ADD CONSTRAINT \"" + table.getName() + "_pk\" PRIMARY KEY (\"installed_rank\")" +... + ";\n" +
    "CREATE INDEX \"" + table.getName() + "_s_idx\" ON " + table + " (\"success\")" + tablespace + ";";
```

Rendered as PostgreSQL DDL (from `projects/flyway.md` — confidence: high):

```sql
CREATE TABLE flyway_schema_history (
  "installed_rank" INT     NOT NULL,
  "version"    VARCHAR(50),        -- NULL for repeatable migrations
  "description"  VARCHAR(200) NOT NULL,
  "type"      VARCHAR(20) NOT NULL,   -- e.g. SQL, BASELINE, SCHEMA, DELETE
  "script"     VARCHAR(1000) NOT NULL,   -- filename, abbreviated if >1000 chars
  "checksum"    INTEGER,          -- CRC-32 signed int, nullable
  "installed_by"  VARCHAR(100) NOT NULL,   -- DB current_user by default
  "installed_on"  TIMESTAMP  NOT NULL DEFAULT now(),
  "execution_time" INTEGER   NOT NULL,   -- milliseconds
  "success"    BOOLEAN   NOT NULL
);
ALTER TABLE flyway_schema_history
  ADD CONSTRAINT flyway_schema_history_pk PRIMARY KEY ("installed_rank");
CREATE INDEX flyway_schema_history_s_idx
  ON flyway_schema_history ("success");
```

Important design notes:
- PK is on `installed_rank` (monotonic surrogate), not on `version` — this is deliberate so the append-only DELETE-tombstone pattern works without unique constraint violations.
- `version` is nullable (NULL for repeatable migrations and tombstone rows).
- The `success` index exists specifically for `DbRepair`'s `WHERE success = FALSE` filter.
- No unique constraint on `(version)` or `(script)` by design.
- Source: `projects/flyway.md` (from `PostgreSQLDatabase.java:56-76`).

### Liquibase's `DATABASECHANGELOG`

The DDL is assembled programmatically from `CreateDatabaseChangeLogTableGenerator.java:47-66`. For PostgreSQL (from `projects/liquibase.md` — confidence: high, reconstructed from generator source):

```sql
CREATE TABLE public.databasechangelog (
  ID      VARCHAR(255) NOT NULL,
  AUTHOR    VARCHAR(255) NOT NULL,
  FILENAME   VARCHAR(255) NOT NULL,
  DATEEXECUTED TIMESTAMP  NOT NULL,
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

There is no primary key declared in DDL. Uniqueness of `(ID, AUTHOR, FILENAME)` is enforced entirely in application code via `RanChangeSet.isSameAs`. `MD5SUM VARCHAR(35)` accommodates the `V:hex` format (e.g. `9:2cdf9876e74347162401315d34b83746` — 1 digit + colon + 32 hex chars = 34, sized to 35). Source: `projects/liquibase.md` (from `CreateDatabaseChangeLogTableGenerator.java:47-66`).

### Liquibase's `DATABASECHANGELOGLOCK`

DDL from `CreateDatabaseChangeLogLockTableGenerator.java:23-41`. For PostgreSQL (from `projects/liquibase.md` — confidence: high):

```sql
CREATE TABLE public.databasechangeloglock (
  ID     INT     NOT NULL PRIMARY KEY,
  LOCKED   BOOLEAN   NOT NULL,
  LOCKGRANTED TIMESTAMP,
  LOCKEDBY  VARCHAR(255)
);
```

Initialised with a single row: `INSERT INTO databasechangeloglock (ID, LOCKED) VALUES (1, false)`. Lock acquisition is `UPDATE... SET LOCKED=true WHERE ID=1 AND LOCKED=false`; the row-count check (must be 1) is the atomicity mechanism. No auto-release on crash. Source: `projects/liquibase.md` (from `CreateDatabaseChangeLogLockTableGenerator.java:23-41`, `InitializeDatabaseChangeLogLockTableGenerator.java:29-32`).

**Ledger / lock relationship in Liquibase:** The ledger table (`DATABASECHANGELOG`) and the lock table (`DATABASECHANGELOGLOCK`) are separate and structurally unrelated. The lock table is created before the ledger table. Flyway merges both concerns in Postgres via advisory locks (no lock table), but falls back to `InsertRowLock` (a sentinel row in `flyway_schema_history` with `installed_rank = -100`) for non-Postgres databases — so the Postgres case keeps the tables cleanly separated.

### Prisma's `_prisma_migrations`

Verbatim Rust DDL from `prisma-engines-reference` (`schema-engine/connectors/sql-schema-connector/src/flavour/postgres.rs:522-534` — confidence: high, read directly from source):

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

Key design points:
- `id` is `VARCHAR(36)` — a UUID string, not an integer. The UUID is generated at the time the migration is started.
- `checksum VARCHAR(64)` — 64 hex characters, consistent with SHA-256. This is the most precise checksum width of any system surveyed.
- `finished_at` is nullable — NULL while the migration is running, set on completion. `rolled_back_at` nullable — set if the migration is explicitly rolled back via `migrate resolve --rolled-back`.
- `applied_steps_count INTEGER NOT NULL DEFAULT 0` — incremented after each successfully-applied SQL statement within the migration, allowing partial-apply detection. This is unique among surveyed systems.
- No `success` boolean — success is inferred from `finished_at IS NOT NULL AND rolled_back_at IS NULL`.
- No `execution_time` — derivable as `finished_at - started_at` but not stored as a column.
- Source: `prisma-engines-reference/schema-engine/connectors/sql-schema-connector/src/flavour/postgres.rs:522-534`. (This was the engine-internal DDL the TypeScript project note could not locate.)

### Diesel's `__diesel_schema_migrations`

Verbatim SQL from `diesel/src/migration/setup_migration_table.sql:1-4` (from `projects/diesel.md` — confidence: high):

```sql
CREATE TABLE IF NOT EXISTS __diesel_schema_migrations (
    version VARCHAR(50) PRIMARY KEY NOT NULL,
    run_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
```

The same SQL is shared across PostgreSQL, MySQL, and SQLite. `version` is a timestamp-derived string (e.g. `20151219180527`), extracted from the directory name by stripping the `_name` suffix. On revert, the row is `DELETE`d — there is no history. No checksum, no execution time, no description. Source: `projects/diesel.md` (from `diesel/src/migration/mod.rs:185`, `setup_migration_table.sql:1-4`).

### SeaORM's `seaql_migrations`

The entity definition drives DDL generation at runtime via `Schema::create_table_from_entity(seaql_migrations::Entity).if_not_exists()`. Verbatim Rust (from `projects/sea-orm.md`, from `sea-orm-migration/src/seaql_migrations.rs:1-15`):

```rust
#[sea_orm(table_name = "seaql_migrations")]
pub struct Model {
  #[sea_orm(primary_key, auto_increment = false)]
  pub version: String,
  pub applied_at: i64,
}
```

Rendered PostgreSQL DDL (from `projects/sea-orm.md` — confidence: high, inferred from entity definition):

```sql
CREATE TABLE IF NOT EXISTS seaql_migrations (
  version  TEXT  NOT NULL PRIMARY KEY,
  applied_at BIGINT NOT NULL
);
```

`applied_at` is a Unix timestamp in seconds stored as `i64`/`BIGINT` — not a `TIMESTAMPTZ`. This is the only system among the eleven that stores the timestamp as an integer rather than a native timestamp type. No checksum, no description, no execution time. Source: `projects/sea-orm.md` (from `seaql_migrations.rs:1-15`, `migrator/exec.rs:196-210`).

### refinery's `refinery_schema_history`

Verbatim SQL DDL from `refinery_core/src/traits/mod.rs:107-112` (from `projects/refinery.md` — confidence: high):

```sql
CREATE TABLE IF NOT EXISTS %MIGRATION_TABLE_NAME%(
     version %VERSION_TYPE% PRIMARY KEY,
     name VARCHAR(255),
     applied_on VARCHAR(255),
     checksum VARCHAR(255));
```

Where `%VERSION_TYPE%` = `int4` by default (or `int8` with the `int8-versions` feature). `%MIGRATION_TABLE_NAME%` = `refinery_schema_history` by default.

Verbatim INSERT query (from `projects/refinery.md`, from `refinery_core/src/traits/mod.rs:95-105`):

```rust
format!(
  "INSERT INTO {} (version, name, applied_on, checksum) VALUES ({}, '{}', '{}', '{}')",
  migration_table_name,
  migration.version(),
  migration.name(),
  migration.applied_on().unwrap().format(&Rfc3339).unwrap(),
  migration.checksum()
)
```

- `applied_on VARCHAR(255)` stores an RFC 3339 timestamp string — not a native `TIMESTAMPTZ`. Same anti-pattern as SeaORM's integer, but as a string.
- `checksum VARCHAR(255)` stores a SipHash-1-3 u64 as a decimal string. Width 255 is massively over-allocated for a 20-digit decimal number.
- No `success` flag, no execution time, no failure state ever written.
- Source: `projects/refinery.md` (from `refinery_core/src/traits/mod.rs:95-112`).

### cot's `cot__migrations`

Defined as a `#[model]`-annotated struct used by the same ORM that manages user tables. Verbatim Rust (from `projects/cot.md`, from `cot/src/db/migrations.rs:1997-2021`):

```rust
#[model(table_name = "cot__migrations", model_type = "internal")]
struct AppliedMigration {
  #[model(primary_key)]
  id: Auto<i32>,
  app: String,
  name: String,
  applied: chrono::DateTime<chrono::FixedOffset>,
}
```

Inferred PostgreSQL DDL (from `projects/cot.md` — confidence: high, from const Operation definition):

```sql
CREATE TABLE IF NOT EXISTS cot__migrations (
  id   SERIAL    PRIMARY KEY,
  app   TEXT     NOT NULL,
  name  TEXT     NOT NULL,
  applied TIMESTAMPTZ NOT NULL
);
```

Structurally identical to Django's `django_migrations` except: cot uses TEXT (unbounded) while Django uses `varchar(255)`, and cot stores a proper timezone-aware timestamp while Django stores `timestamp with time zone`. No checksum, no execution time, no failure flag. Source: `projects/cot.md` (from `migrations.rs:1997-2021`).

---

## Column-by-column analysis

### Version column strategy

Three distinct strategies are in use:

**String version (8 systems):** Django (`name VARCHAR(255)`), Alembic (`version_num VARCHAR(32)`), Flyway (`version VARCHAR(50)`), Diesel (`version VARCHAR(50) PRIMARY KEY`), SeaORM (`version TEXT PK`), refinery (`version int4`), cot (`name TEXT`), Prisma (`migration_name VARCHAR(255)` — this is a name, not an ID). The strings encode a timestamp prefix (`20151219180527`) or a 12-char hex UUID suffix (Alembic) or a sequential number (refinery).

**Surrogate integer sequence (2 systems):** Flyway's `installed_rank INT NOT NULL` is the canonical surrogate integer PK. It is computed as `MAX(installed_rank) + 1` at insert time, not a database sequence. The value is independent of `version` — a BASELINE row, a SCHEMA row, and regular migration rows all share the same monotonic counter.

**UUID string (1 system):** Prisma's `id VARCHAR(36)` is a UUID generated at migration start, not derived from the filename. The migration name is stored separately as `migration_name VARCHAR(255)`.

**Composite natural key (1 system):** Liquibase's `(ID, AUTHOR, FILENAME)` triple is the application-level identity — no single PK column exists in the DDL.

**Implication for Djogi:** The sequential `NNNN_name` scheme means `version` in Djogi's ledger is a string like `"0001_create_users"`. The "version number" part (`0001`) provides monotonic ordering; the name suffix provides human readability. This is closest to Flyway's `installed_rank` (monotonic integer) + `script` (file name) split, but Djogi encodes both in a single column. A surrogate `id BIGINT GENERATED BY DEFAULT AS IDENTITY` alongside the natural `version TEXT` is worth considering to simplify the PK and allow multi-step partial-apply entries.

### Checksum column strategy

| System | Algorithm | Width | Stored as | Null? | What is hashed |
|---|---|---|---|---|---|
| Django | None | — | — | — | — |
| Alembic | None | — | — | — | — |
| Flyway | CRC-32 | `INTEGER` (32-bit signed) | Signed integer | Nullable | File bytes line-by-line (readLine strips newlines) |
| Liquibase | MD5 with version prefix | `VARCHAR(35)` | `"V:hexhex..."` string | Nullable | Parsed Change DSL, not emitted SQL |
| Prisma | SHA-256 (inferred from VARCHAR(64)) | `VARCHAR(64)` | Hex string | NOT NULL | SQL file content (algorithm in Rust engine) |
| Diesel | None | — | — | — | — |
| SeaORM | None | — | — | — | — |
| refinery | SipHash-1-3 | `VARCHAR(255)` | Decimal u64 string | NOT NULL | name + version + SQL (composite hash) |
| cot | None | — | — | — | — |

Critical observations:

1. **Flyway's CRC-32 as signed `INTEGER`** is the worst design among the three. CRC-32 is a 32-bit value; Java casts it to `int` so it can be negative. Comparing across languages requires handling signedness. The collision domain (2^32) is too small for projects with thousands of migrations.

2. **Liquibase's `VARCHAR(35)` with `V:hex` prefix** is the best design for future-proofing. The version prefix (`9:`) allows the algorithm to change without needing a schema migration on the ledger. The downside is that the hash covers the parsed DSL, not the emitted SQL — changing the Liquibase version can change the SQL generator output without changing the hash.

3. **Prisma's `VARCHAR(64) NOT NULL`** is consistent with a SHA-256 hex string. The NOT NULL constraint is correct — a row should never exist without a checksum. This is the most secure choice.

4. **refinery's `VARCHAR(255)`** for a decimal u64 is massively over-allocated but harmless. SipHash-1-3 is not a cryptographic hash; it should not be used for security-sensitive checksums. The inclusion of `name` and `version` in the hash inputs means renaming a migration file changes the checksum even if SQL is unchanged — this is wrong behavior for a content-integrity check.

5. **Djogi's multi-checksum proposal** (`up_checksum`, `down_checksum`, `source_checksum` as separate columns) is unique across all surveyed systems. No prior art splits checksums by migration direction. The rationale is sound (you want to know if `_up.sql` drifted, `_down.sql` drifted, or the Djogi model source drifted independently), but three separate checksum columns add cost. The recommended implementation: use `VARCHAR(64)` hex columns for SHA-256 (matching Prisma's width), adopt Liquibase's `V:hex` prefix pattern (so `up_checksum` = `"1:abcdef..."`), and mark all three NOT NULL.

### Timestamp column strategy

| System | Column name | Type | Timezone? | Generated by |
|---|---|---|---|---|
| Django | `applied` | `timestamp with time zone` | Yes | Application (`default=now`) |
| Alembic | None | — | — | — |
| Flyway | `installed_on` | `TIMESTAMP` | No (no TZ!) | DB default `now()` |
| Liquibase | `DATEEXECUTED` | `TIMESTAMP` | No (MSSQL gets datetime2(3)) | Application |
| Prisma | `started_at` | `TIMESTAMPTZ` | Yes | DB default `now()` |
| Prisma | `finished_at` | `TIMESTAMPTZ` | Yes | Application (set on completion) |
| Diesel | `run_on` | `TIMESTAMP` | No | DB default `CURRENT_TIMESTAMP` |
| SeaORM | `applied_at` | `BIGINT` (unix seconds) | N/A | Application |
| refinery | `applied_on` | `VARCHAR(255)` (RFC 3339 string) | Yes (in string) | Application |
| cot | `applied` | `TIMESTAMPTZ` | Yes | Application |

Key findings:

- **Flyway and Diesel both use `TIMESTAMP` without timezone.** For a system that may be operated globally or across DST boundaries, this is a latent bug. The Liquibase note in `projects/liquibase.md` calls out that `DATEEXECUTED` is second-precision only.
- **Prisma is the only system to store two timestamps** (start and finish), enabling derivation of execution time as `finished_at - started_at`. The `applied_steps_count` column combined with `started_at` and `finished_at` together give a complete execution timeline.
- **SeaORM's `BIGINT` unix seconds** loses sub-second precision and is painful to query with date arithmetic. This is the worst format.
- **refinery's `VARCHAR(255)`** stores an RFC 3339 string — timezone-correct but not queryable as a date without casting.
- **Djogi's `applied_at`** should use `TIMESTAMPTZ` (PostgreSQL native, with microsecond precision), generated by the DB via `DEFAULT now()` as Flyway and Prisma do. The Django note (`projects/django.md`, SURPRISE 2) observes that the timestamp should be recorded after all deferred SQL completes, not after the main DDL body — Djogi should follow this.

### Status and failure flags

| System | How failure is recorded |
|---|---|
| Django | Row never written on failure; row deleted on revert |
| Alembic | Row never written on failure |
| Flyway | `success = FALSE` row written on non-transactional failure; row never written on transactional failure (rolled back); `DELETE` tombstone rows for removed migrations |
| Liquibase | No row written for FAILED or SKIPPED (`MarkChangeSetRanGenerator.java:52-54`) |
| Prisma | Row written at start with `finished_at = NULL`; updated to set `finished_at` on success; `rolled_back_at` set on explicit rollback. Row persists as audit trail. |
| Diesel | Row never written on failure |
| SeaORM | Row never written on failure |
| refinery | Row never written on failure |
| cot | Row never written on failure |

Prisma is the only system with a first-class "migration in flight" state (row exists, `finished_at IS NULL`). All other systems follow the binary pattern: row present = success, row absent = not applied or failed. Flyway's `success = FALSE` row is the one exception but it is only written for non-transactional failures, and the `repair` command removes it before the next run.

The problem with the binary pattern is that a crash after DDL commit but before the ledger write produces an invisible partial application. This window exists in:
- refinery (DDL and ledger INSERT are separate transactions by default)
- cot (DDL and INSERT are unconditionally separate)
- Liquibase (two separate commits: DDL then ledger)

For transactional DDL on PostgreSQL, this window is eliminated because the ledger INSERT is inside the same transaction as the DDL. For non-transactional DDL (`CREATE INDEX CONCURRENTLY`), no system solves this completely.

**Djogi's `status` column** should be an enum stored as `VARCHAR(20)`: `pending`, `applied`, `failed`, `rolled_back`. Writing a row with `status = 'pending'` at migration start (before DDL) eliminates the crash-window problem, because on restart the runner can see the dangling `pending` row and know the migration needs attention.

### Execution metadata columns

| System | `installed_by` / `applied_by` | `execution_time_ms` | Notes |
|---|---|---|---|
| Flyway | `installed_by VARCHAR(100) NOT NULL` | `execution_time INTEGER NOT NULL` | `installed_by` defaults to `current_user` via `Database.java:428-432` |
| Liquibase | — | — | Lock table has `LOCKEDBY VARCHAR(255)` with host info |
| Prisma | — | — | Derivable from `finished_at - started_at` |
| Diesel | — | — | — |
| SeaORM | — | — | — |
| refinery | — | — | — |
| cot | — | — | — |

Only Flyway stores the executing user and execution duration as first-class columns. The Liquibase `LOCKEDBY` pattern (storing `hostname + description + ip`) is used only in the lock table, not the ledger.

**Flyway's `installed_by` design** is cited in `projects/flyway.md` as an adopt: it uses `Database.java:428-432` which queries `current_user` from the database itself, not from the application environment. This avoids misconfiguration from environment variables and gives the actual Postgres role name. Djogi's `applied_by` should do the same: `SELECT current_user` at migration runtime.

For `execution_time_ms`, Flyway stores it as `INTEGER NOT NULL` — milliseconds as a signed 32-bit integer. This overflows at ~24.8 days. Djogi should use `BIGINT` or `INTEGER` if execution times are expected to be under 2.1 billion ms (about 24 days). In practice, `INTEGER` is fine for migration execution times; `BIGINT` is the safe choice.

### `DEPLOYMENT_ID` — the Liquibase innovation

Liquibase's `DEPLOYMENT_ID VARCHAR(10)` groups all changesets applied in a single `update` invocation. This makes it possible to query `WHERE deployment_id = 'abc123'` to see exactly what landed in one deployment. The 10-character width encodes a timestamp (milliseconds since epoch in base 36 or similar short encoding) — short enough to not be a burden, long enough to identify a deployment run.

No other system has this concept. Djogi does not have it in the proposed schema. This is a gap worth considering: a `run_id` or `deploy_id` column that groups all migrations from one `djogi migrate` invocation would make it easy to audit "what changed in the Friday deploy."

---

## Primary key and index strategy

| System | PK strategy | Secondary indexes |
|---|---|---|
| Django | Surrogate `id SERIAL` | None |
| Alembic | Natural `version_num` PK | None |
| Flyway | Surrogate `installed_rank INT` PK | `(success)` index |
| Liquibase | None (application-level composite key) | None |
| Prisma | UUID string `id VARCHAR(36)` PK | None (not declared in DDL) |
| Diesel | Natural `version VARCHAR(50)` PK | None |
| SeaORM | Natural `version TEXT` PK | None |
| refinery | Natural `version int4` PK | None |
| cot | Surrogate `id SERIAL` PK | None implied |

Systems using a natural PK on the version string depend on the version being unique — which fails for Flyway's repeatable migrations, DELETE tombstones, and BASELINE rows. This is why Flyway introduced the surrogate `installed_rank` — it allows multiple rows with the same `version` value (e.g., a `DELETE` tombstone for `version='1.0'` and the original `SUCCESS` row for `version='1.0'` can coexist).

For Djogi's linear `NNNN_name` scheme with no tombstone rows, a natural primary key on `(version)` is safe. However, if Djogi ever implements Prisma-style "failed rows stay in the ledger," a surrogate PK (`id BIGINT GENERATED BY DEFAULT AS IDENTITY`) is necessary to allow multiple rows for the same version (one `applied`, one `failed`). This is a forward-looking reason to add a surrogate PK even in v1.

The Flyway `(success)` index is the only secondary index among all surveyed systems. It exists for `DbRepair`'s `WHERE success = FALSE` filter. Djogi should create a similar `(status)` partial index: `CREATE INDEX ON djogi_migrations (version) WHERE status != 'applied'` — this makes the "find anything that isn't cleanly applied" query fast without adding an index that's mostly never used.

---

## Auto-create vs explicit init

| System | Behavior when ledger table doesn't exist |
|---|---|
| Django | Creates on first `manage.py migrate` (via ORM schema editor, atomically before any migration runs) |
| Alembic | Creates inline in the `--sql` output when starting from `base`; creates automatically on `upgrade` with a live connection |
| Flyway | Creates automatically on first `migrate`; also created by `baseline` command with a BASELINE marker row |
| Liquibase | Creates automatically on first `update`; `changelog-sync` also creates it |
| Prisma | Rust engine's `create_migrations_table` is called at the start of `applyMigrations` |
| Diesel | `CREATE TABLE IF NOT EXISTS` — idempotent, always attempted before any migration |
| SeaORM | `Schema::create_table_from_entity(...).if_not_exists()` in `install()` — idempotent |
| refinery | `CREATE TABLE IF NOT EXISTS` — always attempted in `assert_migrations_table` before each run |
| cot | `CREATE TABLE IF NOT EXISTS` via `const CREATE_APPLIED_MIGRATIONS_MIGRATION` — run at `MigrationEngine::run()` startup |

No system requires a separate `init` or `setup` command purely to create the ledger table. The `init` commands that exist (Alembic's `alembic init`, SeaORM's `sea-orm-cli migrate init`) scaffold the migration directory on disk, not the DB table.

Flyway's `baseline` command is special — it creates the table AND inserts a BASELINE marker row to declare that the database is at a known starting point. This is a one-time operation for adopting Flyway on an existing database. Djogi's equivalent should be `djogi migrate baseline --version NNNN`, which creates the table if absent and inserts a `status='applied'` row for all versions up to NNNN without executing them.

The `IF NOT EXISTS` pattern (Diesel, SeaORM, refinery, cot) is the correct approach for Djogi. The ledger table creation should be idempotent and should happen atomically before the advisory lock is acquired (or immediately after, to avoid the table creation itself being a race). Note: cot creates the ledger table using the same `Operation::create_model()` mechanism as user migrations, which is elegantly consistent — Djogi could do the same by prepending a fixed "ledger table migration" to every run.

---

## Convergence and divergence

### Universal (all serious systems have this)

- A `version` or equivalent identity column that identifies which migration was applied.
- A `applied_at` / `installed_on` / `run_on` timestamp recording when the migration ran.
- A `CREATE TABLE IF NOT EXISTS` or equivalent idempotent creation step.

### Near-universal (7+ systems)

- A single human-readable description or name alongside the version.
- The DDL + ledger write in the same transaction (Diesel, SeaORM explicitly; Django, Alembic, Flyway for transactional DDL).

### Minority pattern (3-4 systems)

- A checksum column: only Flyway, Liquibase, Prisma, refinery.
- A success/failure flag: only Flyway (`success`), Liquibase (`EXECTYPE`), Prisma (via `finished_at`/`rolled_back_at` nullability).
- A user/author column: only Flyway (`installed_by`), Liquibase (`AUTHOR` as part of composite key, `LOCKEDBY` in lock table).

### Unique to one system

- `applied_steps_count` (Prisma) — partial-apply progress counter within a multi-statement migration.
- `DEPLOYMENT_ID` (Liquibase) — groups all changesets from one invocation.
- Multiple heads (Alembic) — `alembic_version` stores one row per active branch head, not one row per applied migration.
- `V:hex` checksum format with version prefix (Liquibase) — algorithm versioning embedded in stored value.
- Timestamp stored as BIGINT unix seconds (SeaORM) — universally considered an anti-pattern by the other systems.
- Timestamp stored as VARCHAR RFC 3339 string (refinery) — similarly an anti-pattern.

### Sharp divergence points

1. **History log vs current-state pointer:** Alembic's `alembic_version` is a current-state pointer (no history). All others maintain a history log.
2. **Failure recording:** Prisma records failures; Flyway records non-transactional failures; all others leave no trace.
3. **Lock mechanism:** Flyway uses Postgres advisory locks (separate from the ledger); Liquibase uses a separate lock table; all Rust systems (Diesel, SeaORM, refinery, cot) have no lock.
4. **Primary key strategy:** 4 systems use natural PK on version, 3 use surrogate integer, 1 uses UUID string, 1 has no PK.
5. **Checksum width and algorithm:** Flyway (CRC-32, 4 bytes stored as signed INTEGER), Liquibase (MD5, 32 hex chars + version prefix), Prisma (SHA-256, 64 hex chars), refinery (SipHash-1-3, u64 decimal string).

---

## Djogi implications

### Columns that are well-validated by prior art — keep as-is

**`version TEXT NOT NULL`** — every system has this. Use `TEXT` (unbounded) rather than `VARCHAR(50)` to avoid truncation surprises on long migration names, since Postgres stores short strings identically regardless.

**`description TEXT`** — Flyway (`description VARCHAR(200)`), Liquibase (`DESCRIPTION VARCHAR(255)`), refinery (`name VARCHAR(255)`), cot (`name TEXT`) all have this. The `VARCHAR(200)` limit in Flyway is an arbitrary truncation noted in `projects/flyway.md` (SURPRISE 7). Use `TEXT` in Djogi.

**`applied_at TIMESTAMPTZ NOT NULL DEFAULT now()`** — 8 of 11 systems track this. Use `TIMESTAMPTZ` (Flyway and Diesel use plain `TIMESTAMP` which is wrong); use `DEFAULT now()` as a server-side default (Flyway, Prisma do this; it avoids clock skew). Per Django SURPRISE 2 (`projects/django.md`), record the timestamp after all deferred SQL commits, not after the main DDL body.

**`applied_by TEXT`** — Flyway (`installed_by VARCHAR(100)`) is the only system with this, but the rationale is strong: it defaults to `current_user` via a DB-side query, giving the actual Postgres role name regardless of application configuration. Djogi should populate this via `SELECT current_user` at migration time. Worth keeping.

**`execution_time_ms BIGINT`** — Only Flyway has this (`execution_time INTEGER NOT NULL`). Flyway's INTEGER can overflow at ~24 days; Djogi should use BIGINT. Valuable for performance monitoring.

**`status TEXT NOT NULL DEFAULT 'applied'`** — Flyway has `success BOOLEAN`, Liquibase has `EXECTYPE VARCHAR(10)`, Prisma infers from timestamp nullability. Djogi's explicit `status` column as a string enum (`pending`, `applied`, `failed`, `rolled_back`) is the cleanest design. It allows detecting the crash-between-DDL-and-ledger-write window (the row exists with `status = 'pending'`), which no other system except Prisma (via `finished_at IS NULL`) can detect.

### Columns unique to Djogi — scrutinize carefully

**`up_checksum`, `down_checksum`, `source_checksum`** — No other system stores multiple checksums as separate columns. The distinction matters: `up_checksum` verifies the SQL actually applied has not drifted; `down_checksum` verifies the rollback script has not drifted; `source_checksum` (presumably a checksum of the Rust model source) verifies the model definition matches. The multi-checksum design is sound. Use `VARCHAR(64)` for SHA-256 hex. All three should be `NOT NULL` (a missing checksum is a data quality problem, not a valid state). Adopt Liquibase's `V:hex` prefix (`"1:abcdef..."`) so the algorithm can be upgraded without a ledger schema migration.

**`execution_mode TEXT`** — no other system stores this as a ledger column. Flyway uses `type VARCHAR(20)` which partially encodes this (SQL vs BASELINE vs SCHEMA). Liquibase stores `runInTransaction` in the changelog file but not in `DATABASECHANGELOG`. For Djogi, knowing at query time whether a migration ran transactionally or not is valuable for debugging partial-apply incidents. Keep, but make it a constrained string: `CHECK (execution_mode IN ('transactional', 'non_transactional'))`.

**`out_of_order_flag BOOLEAN`** — Flyway records out-of-order application implicitly (the applied row's `installed_rank` is higher than its version order would suggest), but no system stores an explicit boolean for this. The flag is valuable for audit queries: `SELECT * FROM djogi_migrations WHERE out_of_order_flag = true`. Keep as `BOOLEAN NOT NULL DEFAULT false`.

**`partial_apply_state TEXT`** — no other system has this as a ledger column, though Prisma has `applied_steps_count INTEGER DEFAULT 0` which is related. The Djogi `partial_apply_state` column is more valuable because it can encode human-readable state (`null`, `'step_3_of_7_completed'`). However, this is only meaningful for non-transactional migrations. For transactional migrations, the whole migration either completes or rolls back atomically. Recommend: rename to `partial_apply_info TEXT` and populate it only for non-transactional migrations when a failure mid-script is detected.

### Column to drop

**`source_checksum`** — consider making this one column rather than three distinct checksum columns. The `source_checksum` (checksum of the Rust model source) belongs at build time, not in the runtime ledger. The Djogi `build.rs` already produces `target/djogi_models.json`; the checksum of the source model can be stored there or in a sidecar column only if it proves useful for runtime drift detection. As a ledger column that the DB runtime would need to verify, it is unclear what value it adds that `up_checksum` does not already provide. Strong recommendation: **drop `source_checksum` from the ledger** and keep it in `djogi_models.json` as a build artifact.

### Column to add

**`run_id TEXT`** — inspired by Liquibase's `DEPLOYMENT_ID VARCHAR(10)`, a `run_id` column that groups all migrations applied in a single `djogi migrate` invocation would be extremely valuable for production post-mortems. Unlike Liquibase's 10-character truncated encoding, Djogi should use a proper UUID or a timestamp-prefixed nanoid. This allows `SELECT * FROM djogi_migrations WHERE run_id = 'abc123' ORDER BY applied_at` to show everything that landed in one deploy. No system except Liquibase has this; Djogi would be the first in the Rust ecosystem to expose it.

### Recommended Djogi ledger DDL

Based on the cross-project analysis, the following DDL synthesizes the best choices from each system:

```sql
CREATE TABLE IF NOT EXISTS djogi_migrations (
  -- Identity
  id       BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
  version     TEXT  NOT NULL UNIQUE,    -- e.g. "0001_create_users"
  description   TEXT  NOT NULL DEFAULT '',
  
  -- Checksums (SHA-256 hex with V:hex version prefix, NOT NULL once applied)
  up_checksum   VARCHAR(66) NOT NULL,     -- "1:" + 64 hex chars
  down_checksum  VARCHAR(66),          -- NULL if no _down.sql provided
  
  -- Execution tracking
  execution_mode TEXT  NOT NULL DEFAULT 'transactional'
            CHECK (execution_mode IN ('transactional', 'non_transactional')),
  status     TEXT  NOT NULL DEFAULT 'applied'
            CHECK (status IN ('pending', 'applied', 'failed', 'rolled_back')),
  
  -- Timestamps (all TIMESTAMPTZ; applied_at set by DB, not application)
  applied_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  
  -- Attribution
  applied_by   TEXT  NOT NULL DEFAULT current_user,
  
  -- Performance
  execution_time_ms BIGINT NOT NULL DEFAULT 0,
  
  -- Audit flags
  out_of_order_flag  BOOLEAN NOT NULL DEFAULT false,
  partial_apply_info TEXT,           -- NULL for transactional; details for non-transactional failures
  run_id       TEXT            -- UUID grouping all migrations from one invocation
);

-- Fast lookup for "anything not cleanly applied"
CREATE INDEX djogi_migrations_status_idx
  ON djogi_migrations (version)
  WHERE status != 'applied';

-- Fast lookup by deployment run
CREATE INDEX djogi_migrations_run_id_idx
  ON djogi_migrations (run_id)
  WHERE run_id IS NOT NULL;
```

This design:
- Takes Flyway's `installed_by` / `execution_time` / `success` paradigm.
- Takes Prisma's `TIMESTAMPTZ` and `applied_steps`-awareness.
- Takes Liquibase's `DEPLOYMENT_ID` concept (as `run_id`).
- Takes refinery's `V:hex` checksum version-prefix concept.
- Adds Djogi-original `execution_mode`, `out_of_order_flag`, `partial_apply_info`.
- Drops `source_checksum` (build-time artifact, not runtime concern).
- Drops the `down_checksum` / `up_checksum` split in favor of `up_checksum NOT NULL` + `down_checksum` nullable (consistent with `_down.sql` being optional).

---

## Open questions

1. **Surrogate PK vs natural PK:** The recommended DDL above adds `id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY` with `version TEXT NOT NULL UNIQUE`. This is Flyway's approach. The alternative is Django/Diesel's natural key (`version` as PK). The surrogate key is required if Djogi ever allows multiple rows per version (e.g., failed attempts + successful attempt). Decide before schema is frozen.

2. **`partial_apply_info` for non-transactional migrations:** Prisma's `applied_steps_count INTEGER DEFAULT 0` is more machine-readable than a free-text column. Should Djogi use a structured approach (e.g., `partial_apply_step_index INT` + `partial_apply_total_steps INT`) instead of a TEXT field? The answer depends on whether the runner can enumerate statements before execution.

3. **`run_id` type and generation:** Should `run_id` be a UUID (36 chars), a timestamp prefix + random suffix (sortable), or Liquibase's base-36 short encoding (10 chars)? A ULID (26 chars, lexicographically sortable by time) would be a good choice for Djogi's Postgres 18 target.

4. **`down_checksum` nullability semantics:** If `_down.sql` is absent, `down_checksum` is NULL. If `_down.sql` exists but is empty (a stub), what happens? Should an empty `_down.sql` get a checksum of an empty string, or remain NULL? Empty vs absent are semantically different states for a rollback script.

5. **Version prefix for `up_checksum` / `down_checksum`:** The Liquibase `V:hex` format uses a single digit prefix (`"9:"`) to identify the checksum algorithm version. For Djogi's `VARCHAR(66)` = `"1:" + 64 hex chars`, what event triggers version `"2:"`? The algorithm change policy should be documented before the first release so existing ledgers don't need a schema migration to upgrade the checksum format.

6. **`applied_at DEFAULT now()` vs application-generated:** Flyway and Prisma use a server-side `DEFAULT now()`. Django passes the timestamp from Python. The server-side default is safer (no clock skew, no timezone confusion), but it cannot be overridden if you need to replay historical timestamps during a baseline operation. For `baseline --fake`, what value should `applied_at` get?

7. **Index on `(out_of_order_flag)` or partial index:** If the system enforces that out-of-order is rare (rejected by default in CI/prod), a partial index `WHERE out_of_order_flag = true` would never be used in practice. Is it worth creating, or is it noise?

8. **`execution_mode` in ledger vs in migration metadata:** All surveyed systems that have `runInTransaction`-like semantics store the flag in the migration file, not in the ledger. Storing it in the ledger too means auditing "how many non-transactional migrations have been applied?" is a simple SQL query. Confirm this dual-storage is intentional.
