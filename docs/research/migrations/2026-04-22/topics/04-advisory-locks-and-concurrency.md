# Topic 04: Advisory Locks and Concurrency

## Executive Summary

The question: how do migration runners prevent two processes from applying
migrations simultaneously to the same database? A concurrent apply is a
silent data-hazard — both processes read the same "pending" set, both execute
the same DDL, and the second one typically fails with a duplicate-key error
only *after* the DDL has already run twice.

Across eleven prior-art systems the industry splits into four camps:

| Camp | Members |
|------|---------|
| Postgres advisory lock | Flyway (modern default), Prisma |
| Dedicated lock table (boolean-row) | Liquibase |
| No lock | Django, Alembic, refinery, Diesel, SeaORM, cot, SQLAlchemy (not a runner), sea-query (not a runner) |
| Ledger-table LOCK | none in the set (SQLAlchemy `LOCK TABLE` pattern is documented elsewhere but not used by any runner here) |

The split is stark: the two most production-hardened enterprise Java tools
(Flyway, Liquibase) both lock; the lighter-weight and Rust-native tools
universally do not. Prisma is the exception — it is a Rust-backed tool that
does take an advisory lock.

**Djogi leans toward** `pg_advisory_lock(<hashed-key>)`, matching Flyway's
modern default and Prisma's explicit choice. This topic validates that
decision, stress-tests the key-derivation and scope choices, and surfaces the
one nuance that trips up most teams: session-scoped vs transaction-scoped
advisory locks are not interchangeable.


## Comparison Matrix

| System | Strategy | Mechanism | Key | Scope | Auto-release? | Fallback on timeout | Djogi-relevant? |
|--------|----------|-----------|-----|-------|---------------|---------------------|-----------------|
| Flyway (Postgres) | Advisory lock | `pg_try_advisory_lock(lockNum)` + retry | `LOCK_MAGIC_NUM + hashCode(table.toString())` (ASCII "Flyway" + table-name hash) | Session | Yes — on disconnect | Retry per `lockRetryCount`; no fail-fast | Primary reference |
| Flyway (transactionalLock=true) | Advisory xact lock | `pg_try_advisory_xact_lock(lockNum)` + retry | Same key | Transaction | Yes — on COMMIT/ROLLBACK | Same retry | See dedicated section |
| Flyway (non-PG) | InsertRowLock sentinel | INSERT `installed_rank = -100` row; PK uniqueness races | N/A | Connection lifetime | Heartbeat at 10 min; expired rows reaped | Heartbeat / reap | Not applicable (Djogi is PG-only) |
| Prisma (Postgres) | Advisory lock | `SELECT pg_advisory_lock(72707369)` | Hardcoded `72707369` (magic constant) | Session | Yes — on disconnect | 10-second timeout → `DatabaseTimeout` error | Primary reference |
| Prisma (MySQL) | Named lock | `SELECT GET_LOCK('prisma_migrate', 10)` | String `'prisma_migrate'` | Session | Yes | 10-second timeout | Not applicable |
| Prisma (CockroachDB) | No lock | Explicit fallthrough | — | — | — | — | Not applicable |
| Liquibase | Dedicated lock table | `UPDATE DATABASECHANGELOGLOCK SET LOCKED=true WHERE ID=1 AND LOCKED=false` | Row `ID=1` | None (table row persists past disconnect) | No — manual `releaseLocks` or DELETE | 5-minute timeout loop; throws `LockException` | See Approach B |
| Django | No lock | None | — | — | — | Concurrent runs can corrupt ledger | Negative reference |
| Alembic | No lock | None (env.py hook available to user) | — | — | — | Concurrent runs → duplicate-key error on `alembic_version` PK | Negative reference |
| refinery | No lock | None — grep confirmed zero matches | — | — | — | Concurrent runs → duplicate insert after both execute DDL | Negative reference |
| Diesel | No lock | None — grep confirmed zero matches | — | — | — | Concurrent runs → duplicate-key error on `version` PK | Negative reference |
| SeaORM | No lock | None — grep confirmed zero matches | — | — | — | Same as Diesel | Negative reference |
| cot | No lock | None — grep confirmed zero matches | — | — | — | DDL and ledger INSERT in separate transactions; race window | Negative reference |
| SQLAlchemy | Not a runner | N/A | — | — | — | — | Schema-metadata reference only |
| sea-query | Not a runner | N/A | — | — | — | — | DDL-builder reference only |


## Approaches

### Approach A: Postgres Advisory Lock

**Systems:** Flyway (default), Prisma.

**How it works:** Before touching the migration table the runner calls
`pg_advisory_lock(key)` (blocking) or `pg_try_advisory_lock(key)` (non-blocking,
returns boolean). The lock is held on the calling session. Any other session
calling the same function with the same key blocks until the first session
releases the lock. Postgres releases the lock automatically when the session
terminates — whether by normal disconnect, crash, or network timeout.

#### Flyway's implementation

Source: `flyway-database/flyway-database-postgresql/src/main/java/org/flywaydb/database/postgresql/PostgreSQLAdvisoryLockTemplate.java:37-123`

The lock key is constructed as:

```java
private static final long LOCK_MAGIC_NUM =
        (0x46L << 40) // F
                + (0x6CL << 32) // l
                + (0x79L << 24) // y
                + (0x77 << 16) // w
                + (0x61 << 8)  // a
                + 0x79;        // y
```

The effective key per connection is `LOCK_MAGIC_NUM + discriminator`, where
discriminator = `table.toString().hashCode()` (Java's signed 32-bit
`String.hashCode()`), cast to `long`.

`PostgreSQLConnection.java:104-106`:
```java
public <T> T lock(Table table, Callable<T> callable) {
    return new PostgreSQLAdvisoryLockTemplate(
        database.getConfiguration(), jdbcTemplate, table.toString().hashCode()
    ).execute(callable);
}
```

The acquire call is `pg_try_advisory_lock(lockNum)` (session-scoped, non-blocking,
returns boolean). If it returns `false`, Flyway sleeps and retries per
`RetryStrategy` (configurable via `lockRetryCount`). Release is always in a
`finally` block via `SELECT pg_advisory_unlock(lockNum)`.
`PostgreSQLAdvisoryLockTemplate.java:88-122`.

Confidence: **high** (source read in full).

#### Prisma's implementation

Source: `schema-engine/connectors/sql-schema-connector/src/flavour/postgres.rs:363-389`

```sql
SELECT pg_advisory_lock(72707369)
```

The key `72707369` is a hardcoded magic constant. The comment in source reads:
*"It does not have any meaning, but it should not be used by any other tool."*
This is a session-scoped lock. The timeout is `ADVISORY_LOCK_TIMEOUT = Duration::from_secs(10)`.
On timeout, a `DatabaseTimeout` error is returned (not a retry).

`acquire_lock()` is called at the top of `apply_migrations` and `mark_migration_applied`
(the two mutating commands) but **not** on read-only paths like
`diagnose_migration_history` or `evaluateDataLoss`.

Confidence: **high** (source read directly from prisma-engines clone at
`schema-engine/connectors/sql-schema-connector/src/flavour/postgres.rs`).

#### Key derivation strategies in the advisory-lock camp

| System | Key strategy | Key value | Collision risk |
|--------|-------------|-----------|----------------|
| Flyway | "Flyway" magic bytes + Java `String.hashCode()` of table name | Variable per schema-history table name | Two Flyway instances on different databases on same Postgres server could collide if they share a table name — low in practice because schema is included in `table.toString()` |
| Prisma | Hardcoded constant | `72707369` | Any other tool using the same constant would deadlock with Prisma |
| Djogi (planned) | `pg_advisory_lock(x'DJOGMIGR'::bigint)` or SHA-256 of schema name | TBD | Must not collide with Prisma's `72707369` or Flyway's range |

The Djogi plan of hashing the migration pathname or using a constant is sound.
A constant like `pg_advisory_lock(x'DJOGMIGR'::bigint)` converts `DJOGMIGR` as
8 ASCII bytes to the 64-bit integer `0x444A4F474D494752 = 4994068948568834898`.
This is distinct from Prisma's `72707369` and from Flyway's range
(`0x466C7977617900` plus a small discriminator ≈ `19988 billion + small`). There
is no collision between these three in the bigint space.


### Approach B: Dedicated Lock Table

**Systems:** Liquibase.

**DDL** (verbatim from
`liquibase-standard/src/main/java/liquibase/sqlgenerator/core/CreateDatabaseChangeLogLockTableGenerator.java:23-41`):

```java
CreateTableStatement createTableStatement = new CreateTableStatement(...)
        .addPrimaryKeyColumn("ID", ..., int, ..., new NotNullConstraint())
        .addColumn("LOCKED", boolean, ..., new NotNullConstraint())
        .addColumn("LOCKGRANTED", datetime, ...)
        .addColumn("LOCKEDBY", varchar(255), ...);
```

Which resolves for Postgres to:

```sql
CREATE TABLE public.databasechangeloglock (
    ID          INT          NOT NULL PRIMARY KEY,
    LOCKED      BOOLEAN      NOT NULL,
    LOCKGRANTED TIMESTAMP,
    LOCKEDBY    VARCHAR(255)
);
```

The table is initialized with a single row: `ID=1, LOCKED=false`.
`InitializeDatabaseChangeLogLockTableGenerator.java:29-32`.

**Lock acquisition** (`StandardLockService.java:302-366`): issues
`UPDATE DATABASECHANGELOGLOCK SET LOCKED=true, LOCKGRANTED=now(), LOCKEDBY='hostname (ip)' WHERE ID=1 AND LOCKED=false`.
The `AND LOCKED=false` clause is the concurrency safety: only one `UPDATE`
reports `rowsUpdated==1`. The winner calls `database.commit()` to make the lock
visible to other sessions.

**Wait loop** (`StandardLockService.waitForLock`): default wait 5 minutes
(`GlobalConfiguration.java:83`), polling every 10 seconds
(`GlobalConfiguration.java:89`). On timeout: throws `LockException("Could not
acquire change log lock. Currently locked by " + lockedBy)`.

**Crash recovery**: there is **no automatic timeout or auto-release**. If the
process holding `LOCKED=true` crashes, the row stays `true` indefinitely. The
`LOCKGRANTED` timestamp is informational only — Liquibase never compares it to
`now()` to detect stale locks. The only recovery paths are:
1. Run `liquibase releaseLocks` — calls `UnlockDatabaseChangeLogGenerator.java:25-29`
   which issues `UPDATE ... SET LOCKED=false, LOCKGRANTED=null, LOCKEDBY=null WHERE ID=1`
   with no ownership check.
2. Manual `UPDATE databasechangeloglock SET locked=false WHERE id=1`.

The Postgres-aware comment at `StandardLockService.java:153-156` notes the
server will refuse continued use of the same connection after a rollback, so
the `init` retry loop calls `database.rollback()` between attempts.

**Identification string**: the `LOCKEDBY` column records `hostname + hostDescription + " (" + hostaddress + ")"`.
When a lock is stuck, operators can read this to identify the holding process.
This is a genuine UX advantage over advisory locks, where identifying the
holder requires querying `pg_stat_activity`.

Confidence: **high** (source read in full).


### Approach C: Ledger-Table LOCK

**Systems:** None in this set use `LOCK TABLE ... IN EXCLUSIVE MODE` on the
migration ledger itself as the primary concurrency mechanism. The closest
pattern is Flyway's `InsertRowLock` fallback (used on databases without
advisory locks), which inserts a sentinel row with `installed_rank = -100` and
relies on the primary-key uniqueness constraint to serialize concurrent lock
attempts.

`InsertRowLock` (`flyway-core/src/main/java/org/flywaydb/core/internal/database/InsertRowLock.java:52`):
A 10-minute heartbeat refreshes `installed_on`; rows with expired heartbeats
are reaped. This is explicitly **not** used on Postgres (which has advisory
locks), so it is irrelevant to Djogi's Postgres-only design.

The `LOCK TABLE` pattern does appear in some hand-rolled migration setups
(using `LOCK TABLE migration_ledger IN EXCLUSIVE MODE`) but none of the eleven
surveyed systems implements it this way. Not recommended — it holds a table
lock for the entire migration run duration, blocking all reads if `ACCESS
EXCLUSIVE` mode is used.


### Approach D: No Lock

**Systems:** Django, Alembic, refinery, Diesel, SeaORM, cot.

All confirmed by exhaustive grep. Negative evidence is treated as a primary
citation.

#### Django

`django/db/migrations/executor.py` — no lock call found in any migration file.
Lock strategy section in `django.md` states: *"Django has no advisory lock or
distributed lock on migration execution. Multiple concurrent `manage.py
migrate` runs can race and corrupt the applied-state table."*
Confidence: **high** (searched all migration files for lock-related calls).

#### Alembic

Grep across all `alembic/` source files confirmed zero references to
`pg_advisory_lock`, `advisory`, or any explicit mutex.
`alembic.md`: *"Alembic has no built-in advisory locking. A search across all
`alembic/` source files finds zero references to `pg_advisory_lock`, `advisory`,
or any explicit mutex."* The docs recommend wrapping migrations in
`pg_try_advisory_lock(...)` inside `env.py`, but that code lives outside Alembic.
Confidence: **high** (exhaustive grep confirmed zero results).

#### refinery

From `refinery.md`, section "Lock strategy: NONE":
```
grep -rn "advisory\|pg_advisory\|LOCK TABLE\|FOR UPDATE" \
  /home/tarunvir/projects/refinery-reference/ --include="*.rs" --include="*.sql"
```
This grep returns **zero results**. refinery has no advisory lock, no table
lock, and no filesystem lock. There is no documented concurrency guarantee
whatsoever. Confidence: **high** (proved by grep).

#### Diesel

`diesel.md`, section "Lock strategy": *"No PostgreSQL advisory lock. There is
no `SELECT pg_advisory_lock(...)` or `LOCK TABLE` in the migration path. A
search across all `.rs` and `.sql` files in the repository for `advisory_lock`,
`pg_advisory`, `pg_try_advisory`, and `LOCK TABLE` returned zero results."*
The only lock present is a filesystem lock on the migrations directory (via
`fd_lock::RwLock`) scoped to `diesel migration generate` only, not to `run`
or `revert`. Confidence: **high** (proved by grep).

#### SeaORM

`sea-orm.md`, section "Lock strategy": *"No advisory lock. No lock table. No
distributed lock of any kind. Grep across the entire repository for `advisory`,
`pg_try_advisory`, `LOCK TABLE`, `pg_advisory` returns zero results in `.rs`
files."* Confidence: **high** (proved by grep).

#### cot

`cot.md`, section "Lock strategy":
```
grep -rn "pg_advisory\|LOCK TABLE\|pg_try_advisory\|advisory_lock" \
  /home/tarunvir/projects/cot-reference/
# returned zero results
```
Confidence: **high** (proved by grep).

#### Contention behavior without a lock

In the no-lock camp, the "protection" degenerates to primary-key uniqueness
on the ledger table. The sequence of events for two concurrent runners A and B:

1. Both read the ledger — same "pending" set.
2. Both begin executing migration M.
3. Both execute the DDL for M (which may or may not succeed the second time
   depending on whether the DDL is idempotent).
4. Runner A inserts the ledger row first; Runner B gets a duplicate-key error
   on the `INSERT`.

The DDL double-execution is the actual harm. The duplicate-key error on the
ledger INSERT is a noisy but survivable symptom. On non-transactional DDL
this can leave objects in a partially-constructed state that breaks subsequent
migrations.


## Session-Scoped vs Transaction-Scoped Advisory Locks

This is the single most common source of bugs when adopting Postgres advisory
locks for migration serialization.

### The two primitives

Postgres exposes four advisory lock functions:

| Function | Scope | Auto-release trigger |
|----------|-------|---------------------|
| `pg_advisory_lock(key)` | Session | Session terminates (disconnect, crash, idle-session timeout) |
| `pg_try_advisory_lock(key)` | Session | Same; returns false immediately if not available |
| `pg_advisory_xact_lock(key)` | Transaction | COMMIT or ROLLBACK of the enclosing transaction |
| `pg_advisory_xact_lock_shared(key)` | Transaction | Same |

There is **no explicit unlock call** for transaction-scoped locks —
`pg_advisory_unlock()` raises an error if called on a transaction-scoped lock.

### The critical difference for migration runners

A migration runner acquires a lock before the migration body and releases it
after. Consider a non-transactional migration (e.g. one containing
`CREATE INDEX CONCURRENTLY`):

```
ACQUIRE LOCK
DDL statement 1 (non-transactional, auto-commits immediately)
DDL statement 2 (non-transactional, auto-commits immediately)
RELEASE LOCK
```

With a **session-scoped** lock, the lock is held across all of these
auto-committing statements. A second runner cannot enter the migration body
at all.

With a **transaction-scoped** lock, the lock is released at the first
`COMMIT` (which happens after the first non-transactional DDL statement).
The second runner can then acquire the lock and start executing DDL while
the first runner is still executing DDL for the same migration. This is
exactly the race condition the lock was supposed to prevent.

**Conclusion: session-scoped advisory locks are the only correct choice for
migration runners that support non-transactional DDL.**

Flyway's default (`pg_try_advisory_lock`) is session-scoped.
`PostgreSQLAdvisoryLockTemplate.java:95-103`. The `transactionalLock=true`
option switches to `pg_try_advisory_xact_lock`, which is transaction-scoped
and is **only safe when all migrations are transactional**.

### Which systems chose which

| System | Scope | Source |
|--------|-------|--------|
| Flyway (default) | Session | `PostgreSQLAdvisoryLockTemplate.java:95-103` — `pg_try_advisory_lock` |
| Flyway (transactionalLock=true) | Transaction | Same file — `pg_try_advisory_xact_lock` when flag set |
| Prisma | Session | `flavour/postgres.rs:363-389` — `SELECT pg_advisory_lock(72707369)` (blocking, session) |
| Djogi (planned) | Session | `pg_advisory_lock(x'DJOGMIGR'::bigint)` — session-scoped, matching Flyway and Prisma |


## Flyway's `transactionalLock` Option

This is the nuance most teams miss when reading Flyway's advisory-lock design.

### What it does

`flyway.postgresql.transactional.lock` (default: `false`) switches the lock
primitive from `pg_try_advisory_lock` (session-scoped) to
`pg_try_advisory_xact_lock` (transaction-scoped).
Source: `PostgreSQLConfigurationExtension.java:27-34`,
`PostgreSQLAdvisoryLockTemplate.java:95-103`.

When `transactionalLock=true`:
- The lock is acquired at the start of the migration transaction.
- The lock is automatically released when that transaction commits or rolls
  back.
- There is no explicit `SELECT pg_advisory_unlock(...)` call needed (and
  indeed calling it would raise an error).

### When to set `transactionalLock=true`

Only when:
1. All migrations in the run are transactional (no `CREATE INDEX CONCURRENTLY`,
   no `ALTER TYPE ... ADD VALUE` on Postgres < 12, no `VACUUM`, etc.), AND
2. Each migration is wrapped in exactly one transaction.

Under these constraints, the advisory lock is released at transaction commit,
which is fine because the migration is complete at that point.

### When NOT to set `transactionalLock=true`

Any scenario involving non-transactional DDL. From the Flyway "Lessons for
Djogi" section of `flyway.md`: *"Djogi's design `pg_advisory_lock(x'DJOGMIGR'::bigint)`
matches this posture. Rationale: transactional locks release mid-migration if
a statement commits out-of-band, which is wrong for CIC."*
(CIC = `CREATE INDEX CONCURRENTLY`.)
Citation: `flyway-database/flyway-database-postgresql/src/main/java/org/flywaydb/database/postgresql/PostgreSQLAdvisoryLockTemplate.java:100-103`.

### The Flyway "group mode" interaction

With `DbMigrate.isGroup() == true` (`DbMigrate.java:99-104`), the advisory
lock wraps the **entire** migration run (all migrations in one critical
section). With `isGroup() == false` (the default), the lock is acquired
per-migration (`DbMigrate.java:149-153`). In group mode, even with
`transactionalLock=false`, the session-scoped lock is held across multiple
migration transactions, giving stronger serialization.

### Djogi implication

Djogi should use session-scoped locks (the Flyway default, not
`transactionalLock=true`) and should not expose a `transactionalLock`
toggle to users unless a concrete use case arises that requires it. The
session-scoped lock is strictly safer for the general case, particularly
for the non-transactional DDL segments that Djogi's runner supports.


## Stuck-Lock Recovery

### Advisory lock holder dies

#### Session-scoped lock (the good case)

When a Postgres backend holding a session advisory lock terminates — whether
by normal exit, `pg_terminate_backend()`, OOM kill, or TCP reset — Postgres
automatically releases all session-scoped advisory locks held by that backend.
This is a guarantee of the Postgres advisory lock mechanism, not a Flyway or
Prisma implementation choice.

Practical implication: if a migration runner crashes mid-migration, the lock
is released when the backend is reaped. A second runner can then acquire the
lock and attempt to apply the same migration (which may or may not succeed
depending on whether the crashed migration left the schema in a partial state).

This auto-release property is the primary reason session-scoped advisory locks
are superior to dedicated lock-table rows for migration serialization.

#### Connection pool keep-alive complication

Connection pools (e.g., PgBouncer, deadpool-postgres) can hold a connection
open after the application process appears to have died, as long as the pool
manager process is still running. In this scenario the Postgres backend is
still alive (from Postgres's perspective), so the advisory lock is **not**
released. The pool connection is essentially a "zombie" holder.

Djogi uses `deadpool-postgres`. This is a real risk: if the Djogi application
process crashes but the pool manager is still alive, the advisory lock will
remain held until the pool manager either closes the connection or times out.

Mitigations:
1. Use `deadpool-postgres` with `PoolConfig::max_lifetime` set so connections
   are recycled after a bounded time.
2. On startup, check `pg_stat_activity` for any backend holding the Djogi
   advisory lock. If the holding process's `application_name` matches the
   Djogi runner and its state is `idle` (not `active`), it is safe to call
   `SELECT pg_advisory_unlock_all()` on that backend via `pg_terminate_backend()`.
3. Document the `SELECT pg_advisory_unlock(key)` escape hatch (see Djogi
   Implications below).

#### Transaction-scoped lock on crash

If using `pg_advisory_xact_lock` and the transaction is rolled back (e.g., due
to a migration failure on a transactional database), the lock is automatically
released as part of the rollback. This is also reliable. The concern only
applies to session-scoped locks paired with pool keep-alive.

### Lock-table row orphaned (Liquibase pattern)

Liquibase's `DATABASECHANGELOGLOCK` table does not auto-release on crash.
The `LOCKED=true` row persists until:

1. **`liquibase releaseLocks`**: calls `ReleaseLocksCommandStep` →
   `LockService.forceReleaseLock()` → `releaseLock()` which issues
   `UPDATE DATABASECHANGELOGLOCK SET LOCKED=false, LOCKGRANTED=null, LOCKEDBY=null WHERE ID=1`.
   Note: there is **no ownership check** (`UnlockDatabaseChangeLogGenerator.java:25-29`).
   Any process can force-release any other process's lock. This is an
   intentional escape hatch but also a security concern in shared environments.

2. **Manual DELETE / UPDATE**: operators can directly:
   ```sql
   UPDATE databasechangeloglock SET locked = false WHERE id = 1;
   ```

The `LOCKEDBY` column (`hostname + ip`) is the only information available to
diagnose which process is the holder. If the hostname is stale (e.g., a
recycled IP), there is no way to programmatically verify whether the holding
process is still alive without external tooling.

The `LOCKGRANTED` timestamp is stored but **never compared to `now()`** in the
Liquibase source (`StandardLockService.java:302-366`). A lock that was granted
10 days ago due to a crash is treated identically to one granted 10 milliseconds
ago. This is the primary "stale-lock bug class" in the dedicated-lock-table
approach.

Confidence: **high** (read `StandardLockService.java` in full).


## Contention Semantics

### Block indefinitely (default for most that lock)

`pg_advisory_lock(key)` blocks indefinitely by default. Neither Flyway's
default retry path nor Prisma's blocking call has a hard upper bound.

Flyway: `pg_try_advisory_lock` returns `false` immediately, then Flyway retries
in a loop. The retry count is configurable via `lockRetryCount` (default not
capped in source — `RetryStrategy` is constructed with the user-provided count;
if the count is not set, the loop can run indefinitely). The intention is
"wait as long as the other migration run takes."

Prisma: `SELECT pg_advisory_lock(72707369)` is a blocking call with a
`set_statement_timeout` set to `ADVISORY_LOCK_TIMEOUT = 10s` before it
(`postgres.rs:377-383`). Prisma times out after 10 seconds and returns a
`DatabaseTimeout` error.

### Time out (Flyway's lockRetryCount, Prisma's 10-second window)

Flyway with a configured `lockRetryCount` will give up after N attempts.
`PostgreSQLAdvisoryLockTemplate.java:88-93`. The retry interval is controlled
by `RetryStrategy`.

Prisma's 10-second timeout is the hardest time bound in the set. It is also
the most appropriate for CI/CD pipelines where a 10-second wait to detect a
stuck deployment is sufficient signal to fail fast.

### Fail fast (`pg_try_advisory_lock` pattern — no system does this by default)

`pg_try_advisory_lock` returns `false` immediately without blocking. None of
the eleven systems use this in fail-fast mode as their default. Flyway uses
`pg_try_advisory_lock` but wraps it in a retry loop, making the net behavior
equivalent to blocking-with-timeout rather than fail-fast.

A true fail-fast mode (call once, fail if lock not acquired) would be useful
in a CI environment where concurrent migration runs are an error rather than a
normal race to be resolved. No system surveyed implements this as default
behavior.

**Djogi recommendation:** expose a configurable mode:
- `lock_timeout = 0` → fail-fast (single `pg_try_advisory_lock` call)
- `lock_timeout = N` → block up to N milliseconds using `SET lock_timeout = N`
  before `pg_advisory_lock`
- `lock_timeout = None` → block indefinitely (default for backward compat)


## Cross-Datacenter / Multi-Region Considerations

None of the eleven project notes explicitly addressed multi-region or
cross-datacenter migration safety. This section marks the gap.

**Not addressed** in any of: `flyway.md`, `liquibase.md`, `django.md`,
`alembic.md`, `refinery.md`, `diesel.md`, `sea-orm.md`, `sea-query.md`,
`sqlalchemy.md`, `prisma.md`, `cot.md`.

The advisory lock mechanism is connection-local to one Postgres instance. In a
multi-region setup with Postgres streaming replication, the primary holds the
advisory lock; replicas are not writable and therefore cannot acquire a
conflicting lock. This means `pg_advisory_lock` is safe in a primary/replica
topology as long as migrations only run against the primary.

In a multi-primary setup (e.g., Citus, Patroni with active/active, or a
sharded architecture), advisory locks provide no cross-node serialization.
Prisma explicitly falls through to no-lock on CockroachDB
(`flavour/postgres.rs:364-368`) with a comment linking to the CockroachDB
issue tracker. Djogi is Postgres 18 single-primary; this is out of scope for
0.1.0 but should be noted in architecture documentation.


## Convergence / Divergence

**Universal convergence:** every tool that locks at all does so by acquiring
a single serializing primitive before reading the "pending" set. This is the
correct structure: the lock must be acquired before the read, not between the
read and the write, or the check-then-act race is not eliminated.

**Split on mechanism tier:**

- Enterprise Java tools (Flyway, Liquibase) both lock, but via different
  mechanisms. Flyway's advisory lock is strictly superior to Liquibase's
  lock-table row because:
  (a) advisory locks are auto-released on crash; lock-table rows are not.
  (b) advisory locks do not require a separate DDL object in user-accessible
      schema.
  (c) advisory locks are lighter-weight (no row contention on a shared table).

- Rust-native tools (refinery, Diesel, SeaORM, cot) universally skip locking.
  This is not a principled design choice — none of the project notes finds
  a rationale stated in source. It is a gap, consistent with these tools being
  lower-ceremony and often run in contexts where a single deployment pipeline
  prevents concurrent runs organizationally.

- Python tools (Django, Alembic) also skip locking. Alembic's `env.py`
  extensibility allows users to add advisory locks manually, which partially
  mitigates the gap, but this is opt-in behavior not a default.

- Prisma explicitly locks. This is notable: Prisma is a Rust-backed tool
  targeting the same developer audience as Djogi, and it made the same choice
  Djogi is making. This is the strongest affirmation of Djogi's decision.

**The one surprising outlier:** Flyway offers `transactionalLock=true` as an
option. Of all the tools surveyed, Flyway is the only one that exposes the
session-vs-transaction scope distinction as a user-facing configuration knob.
All other locking systems pick one scope and commit to it. This is likely
because Flyway historically used `InsertRowLock` (which has different semantics)
and needed to provide a migration path, not because `transactionalLock=true`
is generally useful.


## Djogi Implications

### Validate the advisory-lock choice

The survey validates `pg_advisory_lock` as the correct choice for Djogi.
Flyway and Prisma — the two systems with the deepest production-at-scale
evidence for Postgres migration locking — both use it. The Rust-native tools
that skip locking do so out of simplicity, not correctness, and their own
project notes flag concurrency as a known gap.

### Key derivation: SHA-256 hash of schema name → bigint

Djogi's planned constant `x'DJOGMIGR'::bigint` is a reasonable approach.
An alternative with better multi-tenant isolation is to hash the schema name:

```sql
-- 8-byte prefix from ASCII "DJOGI" (5 bytes) + schema hash
SELECT ('x' || lpad(
    to_hex(
        ('0x444A4F4749'::bigint << 24) |
        (abs(hashtext(current_schema()))::bigint & 0xFFFFFF)
    ),
    16, '0'
))::bit(64)::bigint;
```

This gives a different lock key per Postgres schema, which means two Djogi
installations on the same Postgres server (different schemas, same database)
get different lock keys and do not interfere with each other.

For 0.1.0 a constant is acceptable. The hashed-key approach is a v2 improvement
when multi-tenant Djogi deployments on shared Postgres infrastructure become
a real use case.

Ensure the chosen constant does not collide with:
- Prisma: `72707369`
- Flyway: approximately `19988 × 10^9 + small` (well outside the u32 range
  that many constants fall in)

### Scope: session (not transaction)

Session-scoped is the correct choice and matches both Flyway's default and
Prisma. The rationale is established in the session-vs-transaction section
above: any Djogi migration run that includes a non-transactional segment (a
`CREATE INDEX CONCURRENTLY`, an `ALTER TYPE ADD VALUE`, or any custom
non-transactional SQL) would have the transaction-scoped lock released before
the migration completes. This is a silent correctness failure.

Implement as:
```sql
SELECT pg_advisory_lock($1);
-- ... all migration work, across any number of transactions ...
SELECT pg_advisory_unlock($1);
```

### Contention: block with configurable timeout, not fail-fast

Default behavior should be to block, matching the dominant pattern. Expose
a `DJOGI_LOCK_TIMEOUT` environment variable (or `lock_timeout` in
`Djogi.toml`) that accepts:
- `0` → fail-fast (`pg_try_advisory_lock`)
- Positive integer → `SET lock_timeout = N; SELECT pg_advisory_lock(key);`
- Absent / `-1` → block indefinitely

For CI/CD pipelines, the recommended setting is `lock_timeout = 30000`
(30 seconds) so a stuck deployment fails within half a minute rather than
blocking the pipeline indefinitely.

### Recovery: document the manual escape hatch

If the Djogi runner process dies while holding the advisory lock and the
deadpool-postgres connection pool keeps the backend alive, operators can
recover by:

```sql
-- Find the backend holding the Djogi advisory lock
SELECT pid, application_name, state, query_start, state_change
FROM pg_stat_activity
WHERE state != 'active'
  AND pid IN (
      SELECT pid FROM pg_locks
      WHERE locktype = 'advisory'
        AND classid = (x'444A4F47'::bigint >> 32)::integer
        AND objid    = (x'444A4F47'::bigint & 0xFFFFFFFF)::integer
  );

-- Terminate the zombie backend (releases the advisory lock)
SELECT pg_terminate_backend(<pid>);

-- Or, forcibly release all advisory locks for a known key:
-- (only use when you are certain the holder is dead)
SELECT pg_advisory_unlock(<key>);
```

The `DJOGI_ADVISORY_LOCK_KEY` constant should be documented in the operator
guide so this query can be run without reading source code.

### Identification: log the holder

Unlike Liquibase's `LOCKEDBY` column, advisory locks do not carry a holder
identification string. When Djogi fails to acquire the lock, it should query
`pg_stat_activity` for the backend holding the advisory lock and include the
`pid`, `application_name`, `state`, and `query_start` in the error message.
This gives operators the same diagnostic information that Liquibase's
`"Could not acquire change log lock. Currently locked by HOST since DATE"`
error message provides.

Example error message template:
```
Failed to acquire Djogi migration lock (key=<N>).
Lock is currently held by:
  pid=<pid>, application_name=<name>, state=<state>,
  lock acquired at=<query_start>
If this process is no longer running, terminate it with:
  SELECT pg_terminate_backend(<pid>);
```


## Open Questions

### Can Djogi detect stuck locks held by dead connections?

Postgres auto-releases advisory locks when the **backend** (server-side
process) terminates. Deadpool-postgres holds connections in a pool on the
client side. If the Djogi *application* process crashes but the pool manager
is still running, the backend remains alive and the lock is not released.

Djogi cannot distinguish "another migration is running" from "a dead process's
pool connection is still open" purely from the advisory lock state. To detect
the latter, Djogi would need to:
1. Query `pg_stat_activity` for the backend holding the lock.
2. Check whether `state = 'idle'` (not actively executing a query) and
   `state_change` is older than the expected maximum migration duration.
3. If both conditions are true, the lock is likely stale and the operator
   should be prompted to terminate the backend.

This logic is non-trivial because the "expected maximum migration duration"
is application-specific. A conservative heuristic: if `state = 'idle'` and
`state_change > 10 minutes ago`, the lock is likely stale.

Implementing this as a diagnostic (Djogi logs a warning but does not
auto-terminate) is safer than auto-terminating, which could interrupt a
legitimately running migration that has a long idle period between statements.

### Should Djogi expose `pg_try_advisory_lock` fail-fast mode in its public API?

The fail-fast mode is useful for tools that want to skip migration execution
if another instance is already running (e.g., a health-check sidecar that
calls `migrate()` on every startup, where it is safe to no-op if another pod
is already migrating). None of the surveyed tools expose this as their default,
but Flyway's underlying use of `pg_try_advisory_lock` makes the primitive
available for users who configure a retry count of zero.

Djogi should expose `RunnerOptions::lock_behavior = LockBehavior::FailFast`
as a named configuration option rather than burying it in a timeout value.

### Is the lock key stable across schema renames?

If Djogi's key is derived from the schema name (`hashtext(current_schema())`)
and the Postgres schema is renamed, the lock key changes. A migration run
started before the rename and completing after would use a different key than
a concurrent run started after the rename. This is an unlikely edge case but
should be documented: **do not rename the Postgres schema during a migration
run**.

If Djogi uses a constant key (`x'DJOGMIGR'::bigint`), this concern disappears.
The tradeoff is that a constant key means two Djogi instances on the same server
in different schemas will contend for the same lock. For the 0.1.0 use case
(single-tenant, single-schema) a constant is the simpler and more predictable
choice.

### How should Djogi behave when the advisory lock returns false immediately?

`pg_try_advisory_lock` returns `false` without blocking if the lock is not
available. The correct behavior depends on context:
- During `cargo djogi migrate` (human-initiated): wait and retry, with a
  human-readable progress message every few seconds.
- During application startup migration (programmatic): either fail fast
  (forcing the operator to retry the deployment) or retry with exponential
  backoff up to a configurable limit.
- During CI: fail fast with a clear error so the pipeline does not hang.

Djogi should surface `LockBehavior` as a first-class type with at least three
variants: `Block`, `RetryWithTimeout(Duration)`, and `FailFast`.
