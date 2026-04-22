# Topic 03: Checksums and Repair

## Executive summary

Across the eleven migration systems surveyed, checksum practice spans a wide range: from no checksum at all (Django, Alembic, Diesel, SeaORM, cot) to weak non-cryptographic hashes (Flyway's CRC-32, refinery's SipHash-1-3) to cryptographic SHA-256 (Prisma) to versioned MD5 (Liquibase). The systems without checksums form the majority — five of the eleven — and all of them silently accept post-apply mutation of migration files. The two systems with credible integrity guarantees are Prisma (SHA-256 over raw SQL bytes, line-ending normalised) and Liquibase (MD5 wrapped in a version-prefixed `V:hex` format string). Flyway's CRC-32 is better than nothing but carries meaningful collision risk at scale and is entirely non-cryptographic.

The repair story is equally uneven. Flyway has the most complete repair semantics: a single `repair` command that removes `success=false` rows, inserts DELETE tombstones for missing migrations, and in-place-updates checksum drift — while explicitly refusing to delete successful rows or re-execute SQL. Prisma's `migrate resolve` is narrower but cleaner: two flags (`--applied`, `--rolled-back`) that drive a well-defined state machine without any mutation of previously-successful rows. Liquibase's `clearChecksums` is a blunt instrument: a single `UPDATE … SET MD5SUM = NULL` with no filtering, no audit trail, no dry-run. Alembic, Django, Diesel, SeaORM, refinery, and cot have no repair command at all.

For partial-apply handling, Flyway is the only system that records a `success=false` row in its ledger when a non-transactional migration fails mid-execution. Prisma records `applied_steps_count` in its ledger row so the operator knows how many DDL statements executed before the failure. Every other system's ledger is all-or-nothing: if the migration fails, no row is inserted, and recovery requires manual inspection. Djogi's planned multi-checksum + partial-apply column approach addresses the most significant gaps in the entire surveyed landscape.

---

## Comparison matrix

| System | Algorithm | What is hashed | Normalization | Versioned format | Repair command | Baseline / stamp | Fake / mark-applied | Partial-apply handling |
|---|---|---|---|---|---|---|---|---|
| **Flyway** | CRC-32 | Raw SQL bytes, line endings stripped via `readLine()`, BOM stripped from first line | Line endings + BOM only | No | `repair` — removes `success=false` rows, inserts DELETE tombstones, in-place updates checksum drift | `baseline` — inserts `type='BASELINE'` row with NULL checksum | `skipExecutingMigrations` (programmatic only, no CLI verb) | `success=false` row written for non-transactional failures; transactional failures roll back with no row |
| **Liquibase** | MD5 (MD5Util) wrapped in versioned format `V:hex` | Parsed Change DSL (not emitted SQL); line endings normalised, Unicode replacement char stripped, NFC-normalised | Line endings + Unicode normalisation | Yes — `V9:hex`, V1–V9 (`ChecksumVersion.java:14-22`) | `clearChecksums` — blind `UPDATE … SET MD5SUM = NULL` | `changelog-sync` / `changelog-sync-to-tag` — writes `EXECTYPE='EXECUTED'` rows without running changes | Same as baseline — `changelog-sync` writes ledger row blind | No partial-apply state; `FAILED`/`SKIPPED` exec-types never written (`MarkChangeSetRanGenerator.java:52-54`) |
| **Prisma** | SHA-256 | Raw SQL bytes of `migration.sql`, line endings normalised in comparison (both `\r\n`→`\n` and `\n`→`\r\n` attempted) | Bidirectional line-ending normalisation at comparison time | No | `migrate resolve` — `--applied` / `--rolled-back` flags drive a state machine | `migrate resolve --applied <name>` after `migrate dev --create-only` | `markMigrationApplied` RPC | `applied_steps_count` column tracks DDL statements completed; failed row stays with `finished_at IS NULL` |
| **Alembic** | None | N/A | N/A | N/A | None | `stamp` / `stamp --purge` | `stamp <revision>` writes revision ID without running migration | None — `alembic_version` has only `version_num` column |
| **Django** | None | N/A | N/A | N/A | None | `--fake-initial` — introspects live DB for table/column existence | `--fake` — records row without running migration | None — `django_migrations` has no error column |
| **Diesel** | None | N/A | N/A | N/A | None | None (manual INSERT required) | None | None — `__diesel_schema_migrations` has no error column |
| **SeaORM** | None | N/A | N/A | N/A | None | None | None | None — `seaql_migrations` has no error column |
| **refinery** | SipHash-1-3 (`SipHasher13`) | `name` + `version` + `sql` (raw `&str`, no normalisation) | None — raw `&str` bytes | No — stored as decimal `u64` string | None | None (manual INSERT required) | `Target::Fake` / `Target::FakeVersion(v)` | None — no `success` column; failed migrations not recorded |
| **cot** | None | N/A | N/A | N/A | None | None | None | None — `cot__migrations` has no error column |
| **sea-query** | N/A (builder only) | N/A | N/A | N/A | N/A | N/A | N/A | N/A |
| **SQLAlchemy** | N/A (schema layer only) | N/A | N/A | N/A | N/A | N/A | N/A | N/A |

---

## Algorithm landscape

### SHA-256-class (strong)

**Prisma** is the only surveyed system using a cryptographic hash. The algorithm and the exact source:

```rust
// prisma-engines-reference/schema-engine/connectors/schema-connector/src/checksum.rs:43-48
fn compute_checksum(script: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(script);
    hasher.finalize().into()
}
```

The output is a 64-character lowercase hex string (`CHECKSUM_STR_LEN = 64`). The hashed input is the raw SQL bytes of `migration.sql`. Line endings are not pre-normalised before hashing; instead, `script_matches_checksum` attempts the hash against the original script, then with `\r\n`→`\n`, then with `\n`→`\r\n` (`checksum.rs:21-23`). This three-attempt strategy means a checksum recorded on a Linux system will validate correctly if the file later gets Windows line endings and vice versa.

Confidence: **high** — read directly from `prisma-engines-reference/schema-engine/connectors/schema-connector/src/checksum.rs`.

There is one backward-compatibility wrinkle: an earlier version of Prisma's schema engine omitted zero-padding in `format_checksum` (issue #1887 referenced at `checksum.rs:27-36`). The comparison function handles both the padded (current) and unpadded (legacy) format by checking `checksum.len() != CHECKSUM_STR_LEN` and falling back to `format_checksum_old()` which writes `{byte:x}` instead of `{byte:02x}`. This is a real-world example of forward-compatibility logic in a checksum format — though not via a version prefix, rather via length detection.

Confidence: **high** — read from `checksum.rs:30-39`.

Why SHA-256? No source-level rationale is given. The `sha2` crate is a well-audited Rust implementation. SHA-256 produces a 256-bit output giving a collision space of 2^256, effectively eliminating accidental collisions for any conceivable migration corpus, and providing at least nominal protection against deliberate tampering.

### CRC-32-class (drift-tolerant but collision-prone)

**Flyway** uses CRC-32, computed line-by-line with line terminators stripped:

```java
// flyway-reference/flyway-core/src/main/java/org/flywaydb/core/internal/resolver/ChecksumCalculator.java:64-87
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
    } catch (IOException e) { ... }
    return (int) crc32.getValue();
}
```

Confidence: **high** — read from `ChecksumCalculator.java:64-87`.

The cast to Java `int` at `(int) crc32.getValue()` means the stored value can be negative, and it is stored in a signed `INTEGER` column (`PostgreSQLDatabase.java:67`). The collision domain is 2^32 (~4 billion). With thousands of migrations across many projects, accidental CRC-32 collisions are unlikely but non-zero. More critically, CRC-32 is not a cryptographic hash: an adversary can trivially construct an altered migration that produces the same CRC-32 as the original.

Liquibase also historically used an MD5-based scheme but wraps it in a version prefix rather than raw bytes. From a collision-resistance perspective, MD5 is stronger than CRC-32 (2^128 collision space) but is cryptographically broken for collision attacks.

### SipHash-1-3 (refinery)

**refinery** uses `SipHasher13` from the `siphasher` crate:

```rust
// refinery-reference/refinery_core/src/runner.rs:92-96
let mut hasher = SipHasher13::new();
name.hash(&mut hasher);    // migration name string
version.hash(&mut hasher); // migration version integer
sql.hash(&mut hasher);     // full SQL content as &str
let checksum = hasher.finish(); // u64
```

Confidence: **high** — read from `refinery_core/src/runner.rs:92-96`.

SipHash-1-3 is a keyed hash designed for hash-table DoS resistance. It is decidedly non-cryptographic but has much better collision resistance than CRC-32 in practice. The key insight from the source comment at `runner.rs:84-91`: refinery explicitly switched from `std::collections::hash_map::DefaultHasher` (which has no stability guarantee across Rust versions) to `SipHasher13` (which is stable) precisely to avoid breaking existing migration checksums across Rust upgrades. This is a pragmatic choice — not a security-driven one.

The checksum is stored as a plain decimal `u64` string in a `VARCHAR(255)` column (`traits/mod.rs:107-112`). No version prefix.

A critical weakness: SipHash-1-3 hashes the raw `&str` bytes without line-ending normalisation. On Windows, `fs::read_to_string` preserves `\r\n`, while `include_str!` (used by `embed_migrations!`) normalises to `\n`. A migration embedded in a binary compiled on Linux will produce a different checksum than the same migration loaded from disk on Windows via the CLI (`runner.rs` notes this). Confidence: **high** — sourced from `runner.rs:197` note.

### MD5 (legacy / Liquibase)

**Liquibase** uses MD5 for its actual digest algorithm, wrapped in a format-versioned string. The relevant source:

```java
// liquibase-reference/liquibase-standard/src/main/java/liquibase/change/CheckSum.java:85-91
public static CheckSum compute(String valueToChecksum) {
    return new CheckSum(MD5Util.computeMD5(
        StringUtil.standardizeLineEndings(valueToChecksum) // normalizes \r\n -> \n
            .replaceAll("�", "")                      // strips Unicode replacement char
            // ... NFC normalization
    ), Scope.getCurrentScope().getChecksumVersion().getVersion());
}
```

Confidence: **high** — read from `CheckSum.java:85-91`, `ChecksumVersion.java:14-22`.

The stored format is `<version>:<hex-md5>`, e.g. `9:2cdf9876e74347162401315d34b83746` (`CheckSum.java:124-126`). The column `MD5SUM VARCHAR(35)` accommodates this: 1 digit + `:` + 32-hex-char = 34 characters, padded to 35.

MD5 is cryptographically broken for collision resistance (Flame malware demonstrated collisions in 2012). However, the attack scenarios for migration checksums are narrow: an adversary would need to craft a SQL file that produces the same MD5 as the original and performs different DDL. In practice, this is a theoretical risk, not a practical one for most deployments. For a security-conscious system, SHA-256 is strictly better. The version-prefix wrapper — the more important design pattern — is discussed in detail below.

**Prisma also uses MD5 in an unrelated context:** the `migration_lock.toml` file pins the connector type string (e.g. `provider = "postgresql"`) but does not use MD5 for this. The `downloadZip.ts:42-47` SHA-256 logic in the TypeScript wrapper verifies the engine binary download, not migration scripts. Confidence: **high** — no MD5 in migration checksums for Prisma; SHA-256 is the migration checksum algorithm.

### None

Five of the eleven systems store no checksum at all:

- **Django** — `django_migrations` table has only `id`, `app`, `name`, `applied` (`recorder.py:32-46`). No hash field. A migration file can be edited after application without detection. Confidence: **high**.
- **Alembic** — `alembic_version` has only `version_num VARCHAR(32)`. No checksum, no history. Confidence: **high** (schema read from `ddl/impl.py:151-183`).
- **Diesel** — `__diesel_schema_migrations` has only `version` and `run_on`. Zero mentions of `checksum`, `hash`, `sha`, `md5`, or `fingerprint` across all migration source (`diesel-reference/diesel_migrations/`). Confidence: **high** (proved by grep).
- **SeaORM** — `seaql_migrations` has only `version` and `applied_at` (`seaql_migrations.rs:6-10`). No hash computation in `exec.rs`. Confidence: **high** (proved by grep).
- **cot** — `cot__migrations` has only `id`, `app`, `name`, `applied` (`migrations.rs:1997-2021`). Grep for `checksum|hash|sha|md5|crc` across all Rust files returns only auth-related blake3 — zero migration hits. Confidence: **high**.

The consequence in all five cases is identical: if a developer edits an applied migration file and someone else runs `migrate` (or `manage.py migrate`, or `cargo run …`) on a database where that migration was already applied, the system will either silently treat it as applied-already or re-run it depending on whether the version string is in the ledger. There is no integrity alarm.

---

## What gets hashed

### Raw SQL bytes

**Prisma** hashes the raw SQL bytes of `migration.sql`, with line-ending normalisation applied only at comparison time (not before hashing). The input to `sha2::Sha256::update(script)` is the raw `&str` content of the file (`checksum.rs:46`). This is the most honest approach: the checksum captures exactly what the file contains, without any transformation that could hide meaningful differences.

### Normalized SQL (line endings, BOM)

**Flyway** normalises before hashing by using Java's `BufferedReader.readLine()` which strips the line terminator from each line, then hashing the line bytes without the terminator (`ChecksumCalculator.java:71-82`). Additionally, the BOM is stripped from the first line (`BomFilter.FilterBomFromString`). This makes the CRC-32 value identical for files that differ only in `\n` vs `\r\n` line endings — a practical benefit for cross-platform teams. However, Flyway makes no effort to normalise whitespace within lines, strip comments, or canonicalise SQL tokens. A single trailing space on any line changes the checksum.

**Liquibase** uses `StringUtil.standardizeLineEndings` which replaces `\r\n` with `\n` via a regex pipeline, strips the Unicode replacement character `�`, and applies NFC normalisation (`CheckSum.java:86-93`). This is the most thorough normalisation in the surveyed set.

### SQL + name + version (refinery)

**refinery** is unique in hashing not just the SQL content but also the migration name and version number:

```rust
// refinery-reference/refinery_core/src/runner.rs:92-96
name.hash(&mut hasher);
version.hash(&mut hasher);
sql.hash(&mut hasher);
```

Confidence: **high** — read from source.

This design choice means that renaming a migration file (which changes `name`) will break its stored checksum even if the SQL is identical. The `runner.rs` "Lessons for Djogi" section explicitly calls this out as something to reject: `refinery_core/src/runner.rs:373`. For Djogi, hashing only SQL content (with name and version excluded) allows safe file renames without checksum invalidation.

### Structured checksum (Liquibase — DSL-level, not bytes)

Liquibase's checksums cover the **parsed Change DSL**, not the emitted SQL. From `ChangeSet.generateCheckSum` at `liquibase-standard/src/main/java/liquibase/changelog/ChangeSet.java:396-422`:

> The builder concatenates `change.generateCheckSum() + ":"` for each `Change` in the changeset, then `visitor.generateCheckSum() + ";"` for each `SqlVisitor`. Each `Change`'s own checksum is assembled from its serialised form (via reflection over `DatabaseChangeProperty` getters) — so the checksum covers the parsed Change DSL, not the emitted SQL.

Confidence: **high** — sourced from `ChangeSet.java:396-422`.

The implication: upgrading Liquibase to a newer version with an improved SQL generator for the same Change DSL will NOT trigger a checksum mismatch. Conversely, changes to the DSL representation of a changeset (e.g., renaming an attribute) will trigger a mismatch even if the emitted SQL is identical. For Djogi — which uses raw SQL files — this design question does not apply. Hashing the raw SQL bytes directly is simpler and more honest.

---

## Normalization matters

Line-ending drift across Windows and Unix environments is a real checksum hazard. Git's `core.autocrlf` setting, Windows text editors, and cross-platform CI pipelines all create scenarios where a file checked in with `\n` is checked out with `\r\n` or vice versa. This is not hypothetical: refinery's project notes explicitly document that the SipHash-1-3 value will differ between a migration embedded via `include_str!` (which normalises to `\n`) and the same file loaded by the CLI on Windows (`fs::read_to_string` preserves `\r\n`) — a silent breakage when teams mix operating systems (`refinery_core/src/runner.rs` note at line 197).

Flyway addresses this by stripping line terminators during hashing (`BufferedReader.readLine()` at `ChecksumCalculator.java:71`). The result is that a file with `\r\n` endings produces the same CRC-32 as the same file with `\n` endings. Prisma addresses it differently: hash the file as-is, but at comparison time try all three variants (original, `\r\n`→`\n`, `\n`→`\r\n`) and accept a match from any (`checksum.rs:21-23`). Liquibase applies `standardizeLineEndings` before hashing.

BOM handling: Flyway strips the UTF-8 BOM from the first line only (`BomFilter.FilterBomFromString` at `ChecksumCalculator.java:76-79`). Liquibase strips the Unicode replacement character but handles BOM separately at the file-read level. Prisma does not address BOM explicitly in the checksum source.

Djogi implication: normalise line endings to `\n` (LF) before hashing. Strip UTF-8 BOM if present. Do not strip trailing whitespace or comments — these are deliberate file content. The normalisation should happen at hash-computation time, not at file-read time, so the stored bytes are unchanged.

---

## Format-versioning: the Liquibase pattern

Of all the patterns surveyed, Liquibase's versioned checksum format is the single most adopt-worthy design decision for Djogi. It solves a problem that every other checksumming system ignores until it becomes a crisis: **what happens when you need to change the hash algorithm?**

### The format

Liquibase stores checksums as `<version>:<hex-digest>`, e.g.:

```
9:2cdf9876e74347162401315d34b83746
```

The version prefix is a decimal integer identifying the checksum algorithm version. The full `VARCHAR(35)` column width accommodates `1 digit + ':' + 32-char hex = 34 characters` (`CreateDatabaseChangeLogTableGenerator.java:47`).

Confidence: **high** — sourced from `CheckSum.java:124-126`, `ChecksumVersion.java:12-22`.

### The version history

Liquibase has maintained nine checksum versions (V1–V9) since the system was created (`ChecksumVersion.java:12-22`):

```java
V9(9, "Version used from Liquibase 4.22.0 till now", "4.22.0"),
V8(8, "Version used from Liquibase 3.5.0 until 4.21.1", "3.5.0"),
V7(7, "Old version", "?"),
...
V1(1, "Pass through version for testing purpose", "0");
```

The V8→V9 transition changed which `Change` objects are included in the checksum: V9 excludes `DbmsTargetedChange` instances that don't match the current database (`ChangeSet.java:404-406`). This is exactly the kind of algorithm change that would break all existing checksums if there were no version prefix — with the prefix, the validator can read the stored version, compare using the matching algorithm, and still identify genuine content changes.

### How validation uses the version

`ShouldRunChangeSetFilter.accepts` reads the stored checksum version (`ValidatingVisitor.java:127-133`, `ShouldRunChangeSetFilter.java:83`) and computes a fresh checksum at the same version for comparison. If the stored version is V8 but the current Liquibase version is V9, it compares using V8. The `upgradeChecksums` path in `AbstractChangeLogHistoryService.java:66-83` then optionally rewrites old-version checksums to the current version in a background pass.

### Why Djogi must adopt this

Without a format prefix, upgrading the hash algorithm requires a database migration of the ledger itself — updating every `checksum` column to the new hash. This is operationally painful and error-prone. With a `V:hex` prefix, the transition is invisible to operators: old rows keep their `V8:hex` values and validate using the V8 algorithm; new rows get `V9:hex` values; the system handles both simultaneously. A maintenance pass can upgrade old rows at leisure.

Djogi's planned multi-checksum design (up-checksum, down-checksum, source-checksum) should apply the version prefix to each checksum independently. Proposed format for Djogi (generalising the Liquibase pattern):

```
V1:<64-char-sha256-hex>
```

Where `V1` identifies the algorithm (SHA-256, normalised line endings, no BOM) and the hex is the 64-character lowercase SHA-256 digest. When SHA-256 is ever superseded by SHA-3 or BLAKE3, existing `V1:hex` rows remain valid and validate using SHA-256; new rows get `V2:hex` and validate using the new algorithm. The ledger column should be `CHAR(67)` or `VARCHAR(70)` to accommodate this format.

---

## Repair commands

### Flyway `repair`

Flyway's `repair` command is the most complete repair implementation in the surveyed landscape. `DbRepair.repair()` at `flyway-reference/flyway-core/src/main/java/org/flywaydb/core/internal/command/DbRepair.java:114-155` performs exactly three operations, in order, within a single transaction:

**Step 1: Remove failed migration rows**

```java
// Database.java:418-426
DELETE FROM <table> WHERE success = FALSE AND (version = ? OR description = ?)
```

Source: `JdbcTableSchemaHistory.java:282-320`. Only rows with `success=false` are deleted. Successful rows are never physically deleted by `repair`. Confidence: **high**.

**Step 2: Mark missing successful migrations as DELETED (tombstone pattern)**

For any applied migration whose source file has vanished from disk (state `MISSING_SUCCESS`, `MISSING_FAILED`, `FUTURE_SUCCESS`, `FUTURE_FAILED`), Flyway calls `schemaHistory.delete(applied)`. Despite the name, this does **not** delete the row — it **inserts a new row** of type `DELETE`:

```java
// JdbcTableSchemaHistory.java:372-399
jdbcTemplate.update(
    database.getInsertStatement(table),
    calculateInstalledRank(appliedMigration.getType()),
    versionObj, appliedMigration.getDescription(), "DELETE", appliedMigration.getScript(),
    checksumObj, database.getInstalledBy(), 0, appliedMigration.isSuccess());
```

Confidence: **high** — read from `JdbcTableSchemaHistory.java:372-399`. The ledger is append-only for non-failed rows. Tombstones are written rather than rows mutated.

**Step 3: Align checksums and descriptions (in-place UPDATE)**

For applied migrations where the resolved checksum, description, or type differs from the on-disk version:

```java
// Database.java:379-386
UPDATE <table>
SET "description" = ?, "type" = ?, "checksum" = ?
WHERE "installed_rank" = ?
```

Source: `JdbcTableSchemaHistory.java:364`. This is the only place repair physically edits an existing row. Flyway refuses to realign synthetic (`BASELINE`, `SCHEMA`, `DELETE`) rows (`DbRepair.java:194,207`) and skips `UNDONE`/`IGNORED` rows. Confidence: **high**.

**What repair explicitly refuses to do:**

- Never drops or truncates the history table (that is `clean`, gated behind `cleanDisabled` at `DbClean.java:63-66`)
- Never deletes a successful row physically
- Never re-executes any SQL
- Repair only touches the ledger

**A significant weakness:** the in-place `UPDATE` on checksum-drift rows silently rewrites history. An operator has no ledger-level record that the checksum was changed and when. For Djogi, this is explicitly listed as something to reject: prefer an append-only log where checksum updates are new rows with a `CHECKSUM_UPDATED` type and a back-reference to the original `installed_rank`. Confidence: **high** — sourced from `JdbcTableSchemaHistory.java:363-368` and Flyway project notes.

### Liquibase `clearChecksums` / `changelogSync`

`ClearChecksumsCommandStep` calls `ChangeLogHistoryService.clearAllCheckSums` which emits:

```sql
UPDATE databasechangelog SET MD5SUM = NULL;
```

Source: `StandardChangeLogHistoryService.java:465-476`. No row filtering, no dry-run, no confirmation, no audit trail. Every row's checksum is wiped regardless of its state. On the next `update`, `upgradeChecksums` walks rows with `NULL` checksums and recomputes them in a per-row `UPDATE databasechangelog SET MD5SUM=? WHERE ID=? AND AUTHOR=? AND FILENAME=?`. Confidence: **high**.

`changelogSync` (the `ChangelogSyncCommandStep`) marks all un-ran changesets as `EXECUTED` without running them. It uses a `ChangeLogSyncVisitor` that calls `database.markChangeSetExecStatus(changeSet, ExecType.EXECUTED)` — writing an `INSERT` with `EXECTYPE='EXECUTED'` for each changeset (`ChangelogSyncCommandStep.java:55-84`). There are no safety checks: Liquibase will blindly mark a changeset as applied even if the corresponding schema object does not exist in the database. Confidence: **high**.

### Prisma `migrate resolve`

`migrate resolve` offers two flags, each driving a distinct code path in `MigrateResolve.ts`:

**`--applied <migration-name>`** calls `markMigrationApplied` RPC (`MigrateResolve.ts:135-140`). In `mark_migration_applied.rs` (Prisma engines reference):

1. Acquires the engine lock (`connector.acquire_lock().await?`)
2. Reads the migration script from the filesystem to compute its checksum
3. Finds all existing ledger rows for the migration name
4. If any row has `finished_at IS NOT NULL` (already succeeded): returns error `MigrationAlreadyApplied`
5. Marks all rows with `finished_at IS NULL AND rolled_back_at IS NULL` as rolled back (`mark_migration_rolled_back_by_id`)
6. Inserts a new row with `started_at = finished_at = now()`, `applied_steps_count = 0`, and the current checksum

Confidence: **high** — read from `schema-engine/commands/src/commands/mark_migration_applied.rs`.

**`--rolled-back <migration-name>`** calls `markMigrationRolledBack` RPC (`MigrateResolve.ts:164-167`). In `mark_migration_rolled_back.rs`:

1. Acquires the engine lock
2. Finds all ledger rows for the migration name
3. If no rows exist: error `CannotRollBackUnappliedMigration`
4. If all rows have `finished_at IS NOT NULL` (all succeeded): error `CannotRollBackSucceededMigration`
5. For rows with `finished_at IS NULL AND rolled_back_at IS NULL`: sets `rolled_back_at = now()`

Source: `schema-engine/commands/src/commands/mark_migration_rolled_back.rs`. Confidence: **high**.

The state machine is clean: `--rolled-back` refuses to touch successful rows (preserving history); `--applied` refuses to re-stamp an already-successful migration. Failed rows gain a `rolled_back_at` timestamp and remain in the ledger as permanent audit trail. This is exactly the posture Djogi should adopt: failed rows stay, retries insert new rows.

**What `migrate resolve` refuses:**

- Refuses to mark a successfully-finished migration as rolled back (error `CannotRollBackSucceededMigration`)
- Refuses to stamp a migration that has already succeeded (error `MigrationAlreadyApplied`)
- Never deletes ledger rows
- Never re-executes SQL

### No repair: Alembic, Django, Diesel, SeaORM, refinery, cot

For these six systems, the recovery workflow is entirely manual:

**Alembic:** Use `alembic stamp <revision>` to write a revision identifier directly to `alembic_version` without running any migration code (`command.py:732-796`). There is no validation — the stamp is blind. For a corrupted checksum (which Alembic cannot detect since it has no checksums), the operator must inspect the live DB and `stamp` to the correct revision.

**Django:** Use `manage.py migrate --fake <app> <migration>` to mark a migration applied without running it (`executor.py:241-266`). Or `--fake-initial` to detect existing tables and fake the initial migration automatically (`executor.py:310-413`). There is no checksum repair because there are no checksums.

**Diesel:** No CLI support for stamping. The operator must manually `INSERT INTO __diesel_schema_migrations (version) VALUES ('...')`. Six CLI variants exist (`Run`, `Revert`, `Redo`, `List`, `Pending`, `Generate`) and none touch the ledger without executing migrations (`diesel_cli/src/migrations/mod.rs:34-192`). Confidence: **high**.

**SeaORM:** No stamp, repair, or fake workflow. The `MigrateSubcommands` enum offers only `Up`, `Down`, `Status`, `Fresh`, `Refresh`, `Reset`, `Init`, `Generate` — none of which mark a migration applied without running it (`sea-orm-cli/src/cli.rs:109-163`). Confidence: **high**.

**refinery:** Has `Target::Fake` and `Target::FakeVersion(v)` in `runner.rs:44-50` — records migrations as applied without executing SQL. CLI flag `-f`. There is no repair command to rewrite checksums. If a checksum drifts (e.g., after an emergency production SQL edit), the only remedy is `set_abort_divergent(false)` (global, noisy) or manual SQL on the ledger. Confidence: **high**.

**cot:** Three CLI commands (`list`, `make`, `new` — `args.rs:46-53`). No stamp, fake, repair, or baseline. Confidence: **high** (proved by source read).

---

## Baseline / stamp / fake

All five concepts share the same semantic: "tell the migration system that this migration was applied without running it." They differ primarily in whether they introspect the live DB before stamping and in what states they leave the ledger.

### Flyway `baseline`

`DbBaseline.baseline()` at `flyway-reference/flyway-core/src/main/java/org/flywaydb/core/internal/command/DbBaseline.java:88-142` operates in three states:

1. **History table does not exist** — create it with a baseline marker (passes `baseline=true` to the DDL generator, which produces an initial `INSERT` via `Database.getBaselineStatement` at `Database.java:388-400`). The marker row has `installed_rank=1`, `type='BASELINE'`, `checksum=NULL`, `success=TRUE`.
2. **History table exists with an existing baseline marker** — if the requested (version, description) matches, no-op; otherwise fail.
3. **History table exists with non-synthetic migrations but no baseline marker** — refuse. Force the user to drop the table or use the rebaselining flow.

The `BASELINE` row type is stored alongside regular migration rows in the same `flyway_schema_history` table, distinguished by `type='BASELINE'`.

Flyway does not have a `stamp` or `fake` CLI verb in the Django sense. Marking an individual migration as applied without running it requires either `skipExecutingMigrations` (programmatic API, not a CLI command) or direct INSERT. The closest is `baseline` itself, which marks everything up to a version as "already applied".

Confidence: **high** — read from `DbBaseline.java:88-142`.

### Alembic `stamp`

`alembic stamp <revision>` writes revision identifiers directly to `alembic_version` without running migration code (`command.py:732-796`, `runtime/migration.py:558-572`). It computes `StampStep` objects from the revision graph — these go through `HeadMaintainer.update_to_step()` exactly like real migration steps, except `StampStep.stamp_revision()` is a no-op for the migration function. The version table row is inserted, updated, or deleted as needed to make the current head match `<revision>`.

`stamp --purge` deletes all rows first (`command.py:759`, `runtime/migration.py:598-601`), which is the "baseline" workflow: bring the database to a known state manually, then `stamp head` to declare it current.

Confidence: **high** — read from `command.py:732-796`.

### Django `--fake` and `--fake-initial`

**`--fake`** — the `MigrationExecutor` calls `record_migration()` directly without calling `migration.apply()`. The `if not fake:` guard at `executor.py:246` skips the actual DDL execution. A row is inserted into `django_migrations` as if the migration ran.

**`--fake-initial`** — triggers `detect_soft_applied()` at `executor.py:310-413` which inspects the live database for table and column existence. If the objects exist, the migration is faked automatically. This is a structured version of baseline adoption: Django will fake the initial migration if the tables it would create already exist.

Key limitation (Surprise 4 from django.md): `detect_soft_applied()` only checks `CreateModel` and `AddField` operations, not constraint or index existence. Djogi's baseline detection may need to be more thorough.

Confidence: **high** — read from `executor.py:241-413`, `migrate.py:52-65`.

### refinery `Target::Fake`

```rust
// refinery-reference/refinery_core/src/runner.rs:44-50
pub enum Target {
    Latest,
    Version(u32),
    Fake,
    FakeVersion(u32),
}
```

`Target::Fake` records all migrations as applied without executing SQL. `Target::FakeVersion(v)` records up to version `v`. The ledger INSERT is written but the migration SQL is not executed. Notably, the `Report` returned has an empty `applied_migrations` list when `Target::Fake` is used — a bug that surprised callers expecting to log what was stamped (`runner.rs` Surprise 2 from the project note).

Confidence: **high** — read from `runner.rs:44-50`.

### Prisma baseline via `migrate resolve --applied`

Prisma's baseline flow (documented in `Baseline.test.ts:23-80`) is:

1. `db pull` to introspect the existing DB into PSL
2. `migrate dev --create-only` to create a migration without applying it
3. `migrate resolve --applied <name>` to stamp it as applied

This is more structured than Alembic's blind `stamp`: Prisma checks the filesystem for the migration script and computes its checksum before inserting the ledger row. The inserted row reflects the actual SQL that would have been applied. Confidence: **high** — sourced from `Baseline.test.ts:23-80` and `mark_migration_applied.rs`.

### Semantic comparison

| Feature | Flyway `baseline` | Alembic `stamp` | Django `--fake-initial` | refinery `Target::Fake` | Prisma `resolve --applied` |
|---|---|---|---|---|---|
| Requires live DB check? | No (DDL-free) | No | Yes (table/column existence) | No | No (reads filesystem) |
| Stores checksum? | No (`BASELINE` row has NULL checksum) | N/A (no checksums) | N/A (no checksums) | Yes (SipHash-1-3) | Yes (SHA-256) |
| Granularity | Up to a version (all-or-nothing) | Any single revision | Only first migration per app | All or up-to-version | Single named migration |
| Refusal if already succeeded? | Yes (case 2) | No (overwrites) | No | No | Yes (`MigrationAlreadyApplied`) |
| Audit trail | `BASELINE` row in history | No row distinction | No row distinction | Indistinguishable from real apply | `started_at = finished_at` distinguishes it |

---

## Partial-apply (migration failed halfway)

### Flyway

Two distinct paths based on whether the migration was transactional:

**Transactional migration (default for Postgres):** The `FlywayMigrateException` throw rolls back everything including any in-progress history write. No row is inserted. The failed migration appears as `PENDING` on the next run. Recovery: fix the migration, re-run.

**Non-transactional migration:** Flyway inserts a `success=false` row so the failure is visible on the next run. Exact code at `DbMigrate.java:258-260`:

```java
schemaHistory.addAppliedMigration(migration.getVersion(), migration.getDescription(),
    migration.getType(), migration.getScript(), migration.getChecksum(), executionTime, false);
```

The `success=false` row is what makes `repair` useful: `DbRepair.removeFailedMigrations` targets exactly `WHERE success = FALSE`. The user must call `repair` before re-running.

Source: `DbMigrate.java:247-263`. Confidence: **high**.

**The SIGKILL gap:** If the process crashes before the catch block (e.g., SIGKILL), no row is inserted at all. The DB state is ambiguous and Flyway on next invocation sees the migration as `PENDING`. There is no resumable-step machinery inside a single script. Confidence: **high**.

### Prisma

Prisma records partial progress via `applied_steps_count` in `_prisma_migrations`. The column is incremented after each DDL statement executes (`sql_migration_persistence.rs:107-108`):

```rust
.set(
    "applied_steps_count",
    Expression::from(Column::from("applied_steps_count")) + Expression::from(1),
)
```

When a migration blows up halfway, the ledger row exists with `finished_at IS NULL` and `applied_steps_count = N` (where N is how many statements succeeded). The row is permanent audit trail — `markMigrationRolledBack` sets `rolled_back_at` but does not delete the row. A retry via `markMigrationApplied` first rolls back the existing failed row and inserts a fresh one.

Source: `sql_migration_persistence.rs:107-108`, `mark_migration_applied.rs`. Confidence: **high**.

### Liquibase

No explicit partial-apply state. If a changeset's DDL commits but the subsequent ledger `INSERT` fails (crash between the two commits — the two-transaction topology described in the project note), Liquibase on next run sees the changeset as un-ran and re-executes it. Within a changeset with `runInTransaction=false`, if some changes succeed and one fails, the ledger gets no row (`MarkChangeSetRanGenerator.java:52-54` returns `EMPTY_SQL` for `FAILED`). Recovery requires `<preConditions onFail="MARK_RAN">` guards. Confidence: **high**.

### Alembic

No tracking at all. `alembic_version` stores only the `version_num` of the completed step. If a migration fails on non-transactional DDL, the version row is not written, but some DDL may have committed. Recovery: inspect manually, complete or revert DDL, then `stamp` to the correct revision (`command.py:732-796`). Confidence: **high** — verified by schema read (`ddl/impl.py:170-183`) and execution path read (`runtime/migration.py:614-633`).

### Django

All-or-nothing: a row is inserted only on successful completion. Non-transactional migration failures leave a partially-applied schema with no record of what succeeded. No repair path (`executor.py:241-266`). Confidence: **high**.

### Diesel

Default: the migration SQL and the ledger `INSERT` are in the same transaction (`migration_harness.rs:186-189`). A failure in a transactional migration rolls back both. For `run_in_transaction = false` migrations (`metadata.toml`), a mid-migration failure leaves the DB partially applied and no ledger row. No partial-apply tracking, no repair. Confidence: **high**.

### refinery

Default (non-grouped) mode: migration SQL and ledger INSERT are **separate transactions**. A crash between them leaves the schema changed with no ledger record. On restart, refinery will attempt to re-run the migration, likely failing on `already exists` errors. No partial-apply state, no repair command. Source: `traits/sync.rs:85-99`, `runner.rs` notes. Confidence: **high**.

### SeaORM and cot

Both write the DDL and ledger INSERT in the same transaction on Postgres (by default). A failure rolls back both. If `use_transaction = Some(false)` (SeaORM) or a Custom operation without a transaction (cot) is used, the same gap applies as Alembic and Django: DDL may be partially applied with no ledger record. Neither has a repair command.

---

## Convergence / divergence

**Universal convergence:** every system that has checksums stores them as hex strings in a `VARCHAR` or `TEXT` column. No system uses a binary column for checksums. The implicit convention (hex text) is universal where checksums exist.

**Universal convergence:** every system with a repair/fake/stamp mechanism requires an explicit operator action — none will silently recover from a partial-apply state.

**Major divergence — algorithm strength:** Prisma (SHA-256) sits at the strong end; CRC-32 (Flyway) and SipHash-1-3 (refinery) are in the middle; MD5 (Liquibase) is cryptographically broken but practically adequate; the majority (five systems) have nothing.

**Major divergence — format versioning:** only Liquibase embeds a version prefix in its stored checksum. Every other checksumming system ties its ledger to a single algorithm with no migration path.

**Major divergence — what is hashed:** Liquibase hashes the parsed DSL (Change-object representation); Prisma and Flyway hash raw file content; refinery hashes content plus metadata (name, version). Raw file content is the honest choice; DSL hashing decouples the checksum from the underlying SQL which is more fragile.

**Major divergence — partial-apply handling:** only Flyway (`success=false` column) and Prisma (`applied_steps_count`, `finished_at IS NULL`) expose partial-apply state in the ledger. All other systems are all-or-nothing.

**Major divergence — repair command completeness:** Flyway's `repair` is the most complete (three operations, refuses to touch successful rows). Prisma's `migrate resolve` is narrower but cleaner (state-machine with refusal semantics). Liquibase's `clearChecksums` is a blunt instrument. The majority have no repair command.

---

## Djogi implications

### Adopt Liquibase's `V:hex` format for all checksum columns

Djogi's planned multi-checksum design (up-checksum, down-checksum, source-checksum) should use versioned format strings from day one. Proposed:

```
V1:<64-char-sha256-hex>
```

Column DDL: `CHAR(67) NOT NULL` (1 char `V` + 1 char digit + 1 char `:` + 64 char hex). For three checksums (up, down, source), that is three `CHAR(67)` columns, or a composite `JSONB` field if the number of checksums needs to grow.

The version prefix is load-bearing: when SHA-256 is eventually superseded, existing rows keep their `V1:hex` values and validate correctly. New rows get `V2:hex`. No data migration of the ledger is required.

Source precedent: `liquibase-reference/liquibase-standard/src/main/java/liquibase/change/CheckSum.java:124-126`, `ChecksumVersion.java:14-22`. Confidence this is the right design: **high**.

### Use SHA-256 over normalised SQL bytes

Adopt Prisma's algorithm: `sha2::Sha256`, input is the SQL bytes with line endings normalised to `\n`, BOM stripped. Store the 64-character lowercase hex string prefixed with `V1:`.

Do not hash migration name or version (reject refinery's approach): a file rename should not break the checksum for an operation that left the DB unchanged.

Do not hash the parsed AST (reject Liquibase's approach): raw SQL bytes are more honest and simpler to implement.

Source: `prisma-engines-reference/schema-engine/connectors/schema-connector/src/checksum.rs:43-48`. Confidence: **high**.

### Adopt Flyway-style `repair` with Prisma-style state machine

Djogi's `repair` command should perform (at minimum):

1. **Delete failed rows** — rows in the ledger with `execution_state = 'failed'` or `partial_apply IS NOT NULL`. These are the equivalent of Flyway's `success=false` rows. Source: `DbRepair.java:282-320`.

2. **Recompute checksums for matching migrations** — for rows where the on-disk SQL has changed checksum since application. This is the equivalent of Flyway's `alignAppliedMigrationsWithResolvedMigrations`. However, unlike Flyway's in-place `UPDATE`, Djogi should **insert a new row** with `execution_state = 'checksum_updated'` and a back-reference to the original row. This makes checksum repairs auditable.

3. **Refuse to re-execute SQL** — repair must only touch the ledger, never the schema. Source principle: `DbRepair.java:114-155`.

4. **Support dry-run** — `djogi migrate repair --dry-run` should report what would change without mutating the ledger. Liquibase's blind `clearChecksums` (`StandardChangeLogHistoryService.java:465-476`) is the anti-pattern to avoid.

5. **Support single-migration targeting** — `djogi migrate repair --migration 0042` should operate on a single migration rather than the entire ledger. Liquibase's full-table `UPDATE MD5SUM = NULL` is too broad.

### Multi-checksum (up, down, source) — is it overbuild?

**No.** Each checksum serves a distinct purpose:

- **up-checksum** — detects post-apply edits to `_up.sql`. This is what Flyway, Prisma, Liquibase, and refinery already provide.
- **down-checksum** — detects post-apply edits to `_down.sql`. No surveyed system checksums the rollback script. A corrupt rollback that silently does the wrong thing on a production rollback is a severe incident. Storing the checksum of `_down.sql` at apply time makes it detectable.
- **source-checksum** — a checksum of the model descriptor snapshot that was the basis for generating the migration. Serves as an integrity check between the descriptor layer and the SQL layer. If someone hand-edits the SQL without regenerating from the descriptor, the source-checksum diverges, which is detectable.

The only overbuild risk is storage: three `CHAR(67)` columns = 201 bytes per migration row. At 10,000 migrations, this is roughly 2 MB of ledger data — negligible.

### Partial-apply column is load-bearing

Djogi's planned `partial_apply` ledger column (or equivalent `applied_steps_count` / `execution_state`) directly addresses the most dangerous gap in six of the eleven surveyed systems. Prisma's `applied_steps_count` column is the most specific implementation. Djogi should store at minimum:

- `execution_state ENUM('applied', 'failed', 'rolled_back', 'checksum_updated')` — maps to Prisma's `finished_at IS NOT NULL`, `finished_at IS NULL`, `rolled_back_at IS NOT NULL`
- `applied_steps_count INTEGER NOT NULL DEFAULT 0` — tracks how many SQL statements completed before a failure, enabling informed recovery

Source: `prisma-engines-reference/schema-engine/connectors/sql-schema-connector/src/flavour/postgres.rs:524-537`, `sql_migration_persistence.rs:107-108`. Confidence: **high**.

---

## Open questions

1. **BLAKE3 vs SHA-256.** BLAKE3 is faster than SHA-256 on modern CPUs and has equivalent security properties for non-adversarial use cases (collision resistance, preimage resistance). The `sha2` crate is the Prisma precedent; the `blake3` crate is maintained by the BLAKE3 team. Is there a concrete reason to prefer one over the other for Djogi's checksum? Both would use `V1:hex` format — switching between them later is safe.

2. **Should the `down-checksum` be computed at apply time or at generate time?** At apply time, the `_down.sql` content is fresh and the checksum is accurate. At generate time (build.rs), the checksum is also accurate. If a developer edits `_down.sql` after generation but before applying, an apply-time checksum captures the edit. A generate-time checksum does not. Recommendation: compute at apply time.

3. **What is the right granularity for `repair --migration`?** Flyway's repair operates on the entire ledger in one transaction. Prisma's `migrate resolve` operates on a single named migration. Djogi's repair should support both: `djogi migrate repair` (full ledger scan) and `djogi migrate repair --migration 0042` (single migration).

4. **How should `migrate resolve --applied` interact with the source-checksum?** When Djogi's `resolve --applied` stamps a migration, it reads the `_up.sql` to compute the up-checksum. Should it also read the `_down.sql` to record the down-checksum, and the descriptor snapshot to record the source-checksum? Recommendation: yes — stamp should record all three checksums so the stamped row is indistinguishable from an organically-applied row in terms of ledger integrity.

5. **Can `clearChecksums` ever be the right command?** Liquibase's `clearChecksums` is a blunt `UPDATE … SET MD5SUM = NULL`. The only justification is "I changed my changeset DSL in a way that changes the checksum but not the effective SQL, and I need to re-sync all checksums." For Djogi (which uses raw SQL, not a DSL), this scenario arises if line-ending normalisation changes (V1→V2 algorithm upgrade). Djogi's `repair --recompute` (recomputes checksums for all applied migrations using the current algorithm and version prefix) is the safe equivalent.

6. **Liquibase's open question from the project note:** does switching from V8 to V9 silently rewrite old ledger rows, or does `ValidatingVisitor` tolerate mixed versions indefinitely? The surveyed source shows `CheckSum.parse` reads the prefix and routes to the matching algorithm (`CheckSum.java:56-68`), but the `upgradeChecksums` rewrite path trigger timing was not fully confirmed. This is worth a follow-up read of `AbstractChangeLogHistoryService.java:66-83` if Djogi adopts a similar upgrade path.
