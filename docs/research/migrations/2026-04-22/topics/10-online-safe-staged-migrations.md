# Topic 10: Online-Safe / Staged Migrations

## Executive summary

The ecosystem universally punts. No surveyed system provides built-in automation for online-safe, zero-downtime schema changes against live production databases. The documented patterns (expand-contract, `CREATE INDEX CONCURRENTLY`, two-phase `NOT VALID` constraint addition) are universally described as "the operator's responsibility." A few systems provide the essential escape hatches — transaction opt-out per migration, a flag for `CONCURRENTLY` index creation — but none assemble these primitives into a guided multi-step workflow.

The split in the ecosystem is between systems that at least acknowledge the problem (Django with `AddIndexConcurrently`, Alembic with `autocommit_block()`, Flyway with per-statement non-transactional auto-detection, SQLAlchemy with `postgresql_concurrently=True`, sea-query with `.concurrently()`) and systems that are completely silent on it (refinery, cot, Diesel, SeaORM, Liquibase, Prisma TypeScript interface).

Prisma's Rust engine goes further than most: it splits migration scripts into individual statements so that `CREATE INDEX CONCURRENTLY` can coexist in a script with other DDL without manual intervention. That is the closest any surveyed system comes to automation, and even that stops short of the full expand-contract lifecycle.

This represents a real opportunity for Djogi. A v0.3+ "online-safe mode" flag that emits two-phase patterns automatically — `CREATE INDEX CONCURRENTLY`, `NOT VALID` + `VALIDATE CONSTRAINT`, expand-contract column sequences — would differentiate Djogi from every system in this survey.

For v0.1, the correct posture is: emit conservative (blocking) SQL by default, provide the `-- djogi:no-transaction` directive for migrations that need `CONCURRENTLY`, and document the expand-contract pattern thoroughly in djogi-book. Do not attempt automation.

---

## Comparison matrix

| System | Built-in online-safe support | CONCURRENTLY by default | Batched backfill | Expand-contract helper | No-transaction flag | Docs |
|---|---|---|---|---|---|---|
| Django | Partial — `AddIndexConcurrently` op (`contrib/postgres/operations.py:123-126`) | No — opt-in op | No | No | `Migration.atomic = False` | Low confidence (docs not read) |
| Alembic | None | No | No | No | `autocommit_block()` context manager | Medium |
| Flyway | None — auto-detects need, does not assist | No — user writes `CONCURRENTLY` | No | No | Per-statement regex detection auto-sets non-transactional | Low (docs sampled) |
| Prisma | None at TS level; Rust engine splits per-statement to allow `CONCURRENTLY` | No — user writes `CONCURRENTLY` in hand-edited SQL | No | No | Per-statement split is automatic | High (verified absence) |
| Liquibase | None | No | No | No | `runInTransaction="false"` per changeset | High (verified absence) |
| Diesel | None | No | No | No | `run_in_transaction = false` in `metadata.toml` | High (verified absence) |
| SeaORM | None | No | No | No | `use_transaction() -> Some(false)` | High (verified absence) |
| sea-query | `CONCURRENTLY` flag on `IndexCreateStatement` — emits string, no transaction enforcement | No | No | No | N/A (query builder, not runner) | High |
| SQLAlchemy | `postgresql_concurrently=True` dialect option + `postgresql_not_valid=True` — DDL layer only | No | No | No | N/A (DDL layer via Alembic) | High |
| refinery | None | No | No | No | None | High (verified absence) |
| cot | None | No | No | No | None | High (verified absence) |

---

## The online-safe problem

### Why it matters

Most DDL on large Postgres tables acquires an `AccessExclusiveLock`. While that lock is held, every `SELECT`, `INSERT`, `UPDATE`, and `DELETE` against the table blocks. On a table with millions of rows and a migration that rewrites every tuple (e.g., `ALTER TABLE ADD COLUMN ... NOT NULL DEFAULT <volatile-expression>` on Postgres < 11, or `ALTER COLUMN TYPE` that requires a cast), the lock may be held for minutes or hours. This is the definition of downtime.

The problem is not hypothetical. The Django, Alembic, Prisma, and Flyway communities all have prominent blog posts and Stack Overflow threads documenting production outages caused by `ALTER TABLE` on large tables. None of the surveyed systems prevent this in their core tooling.

### Postgres lock classification

The following operations hold `AccessExclusiveLock` for the full duration of the operation — blocking all reads and writes:

- `ALTER TABLE ADD COLUMN ... NOT NULL` without a default (Postgres always blocks, even if fast)
- `ALTER TABLE ADD COLUMN ... NOT NULL DEFAULT <volatile or non-constant expression>` — Postgres < 11 rewrites every tuple; Postgres 11+ rewrites only for non-constant defaults
- `ALTER TABLE DROP COLUMN` — fast (mark deleted in catalog), but still takes `AccessExclusiveLock`
- `ALTER TABLE ALTER COLUMN TYPE` with an incompatible type — full table rewrite
- `ALTER TABLE ADD CONSTRAINT ... CHECK ...` — full table scan to validate existing rows
- `ALTER TABLE ADD CONSTRAINT ... FOREIGN KEY ...` — full scan of referencing column
- `CREATE INDEX` (without `CONCURRENTLY`) — blocks writes for the duration of the index build
- `VACUUM FULL` — full rewrite with `AccessExclusiveLock`

The following operations are safe for live tables:

- `ALTER TABLE DROP COLUMN` — fast even on large tables (just marks the catalog; VACUUM FULL eventually reclaims)
- `ALTER TABLE ADD COLUMN ... NULL` (no default, nullable) — catalog-only on Postgres 11+
- `ALTER TABLE ADD COLUMN ... NOT NULL DEFAULT <constant>` — catalog-only on Postgres 11+ (constant stored in catalog, not per-tuple)
- `CREATE INDEX CONCURRENTLY` — non-blocking; requires two passes but holds only `ShareUpdateExclusiveLock` which does not block reads or writes
- `ALTER TABLE ADD CONSTRAINT ... NOT VALID` — fast; skips scan of existing rows; future inserts/updates are validated but existing rows are not
- `ALTER TABLE VALIDATE CONSTRAINT` — takes `ShareUpdateExclusiveLock` only; does not block writes

### The lock_timeout safety net

For any DDL that must acquire `AccessExclusiveLock`, operators commonly set `lock_timeout` before the statement to avoid indefinitely blocking a queue of requests waiting for the lock. The pattern is:

```sql
SET lock_timeout = '2s';
ALTER TABLE foo ADD COLUMN bar TEXT;
RESET lock_timeout;
```

If the lock cannot be acquired within the timeout, the statement fails with an error rather than waiting. The migration must then be retried. None of the 11 surveyed systems emit `SET lock_timeout` automatically before DDL statements. All delegate this to the operator.

---

## The expand-contract pattern

The expand-contract (also called "parallel-change") pattern is the canonical way to rename a column, change a data type, or restructure a table without downtime. It requires coordination between the migration author, the application code, and the deployment pipeline. No surveyed system automates it.

### The six steps

**Step 1 — Expand (add new structure, keep old).**
Add the new column (nullable, no constraints) alongside the old. This is a catalog-only operation on Postgres 11+ and holds `AccessExclusiveLock` only briefly.

```sql
ALTER TABLE orders ADD COLUMN customer_id_new BIGINT;
```

**Step 2 — Deploy application code that dual-writes.**
Before any data migration, deploy the application version that writes to both the old column and the new column on every insert and update. This ensures new rows written during the migration are consistent in both columns.

**Step 3 — Backfill old data.**
Populate `customer_id_new` from `customer_id` for all existing rows. This must be done in batches to avoid a long-running transaction that holds locks and accumulates dead tuples. The canonical batch pattern:

```sql
-- Repeat until rows_updated = 0
UPDATE orders
SET customer_id_new = customer_id
WHERE id BETWEEN :batch_start AND :batch_end
  AND customer_id_new IS NULL;
```

Batch size is tuned by the operator (typically 1000–10000 rows). No surveyed system generates batched backfill SQL. It is always hand-written.

**Step 4 — Add the NOT NULL constraint without a scan.**
Once the backfill is complete and verified, add the constraint using the two-migration `NOT VALID` + `VALIDATE` pattern to avoid a full table scan holding `AccessExclusiveLock`:

```sql
-- Migration A: fast, catalog-only
ALTER TABLE orders ADD CONSTRAINT orders_customer_id_new_not_null
    CHECK (customer_id_new IS NOT NULL) NOT VALID;

-- Migration B: takes ShareUpdateExclusiveLock (non-blocking for reads/writes)
ALTER TABLE orders VALIDATE CONSTRAINT orders_customer_id_new_not_null;
```

After `VALIDATE CONSTRAINT`, Postgres knows the column has no NULLs. You can then safely issue `ALTER TABLE orders ALTER COLUMN customer_id_new SET NOT NULL` — Postgres 12+ is smart enough to use the validated check constraint to skip the scan and convert it to a `NOT NULL` constraint without blocking. This is a three-migration sequence in total, but all three are non-blocking.

**Step 5 — Deploy application code that reads from the new column, stops writing the old.**
This is the cutover point. The application now reads `customer_id_new` only.

**Step 6 — Drop the old column.**
`ALTER TABLE orders DROP COLUMN customer_id` — fast, catalog-only, non-blocking.

Each step corresponds to one or two migration files. The total is four to six migrations for a single column rename or type change. No surveyed system generates this sequence. Django's `SeparateDatabaseAndState` (`operations/special.py:6-61`) can model the database/state divergence during steps 2–5, but the user must author all six migrations manually.

---

## Online-safe operations in Postgres

| Operation | Default lock | Online-safe variant | Pitfall |
|---|---|---|---|
| `CREATE INDEX` | `ShareLock` on table (blocks writes) for full index build | `CREATE INDEX CONCURRENTLY` (`ShareUpdateExclusiveLock` — non-blocking) | Cannot run inside a transaction block; must be in its own migration with `-- djogi:no-transaction` |
| `ALTER TABLE ADD COLUMN` with a non-NULL constant default | `AccessExclusiveLock` brief (Postgres 11+ catalog-only for constants) | Use `NOT NULL DEFAULT <constant>` on Postgres 11+ | Volatile defaults (e.g., `DEFAULT now()`) still rewrite on Postgres < 15; `random()` always rewrites |
| `ALTER TABLE ADD COLUMN` nullable | `AccessExclusiveLock` brief (catalog-only, no default stored) | Already safe on all Postgres versions | No pitfall |
| `ALTER TABLE DROP COLUMN` | `AccessExclusiveLock` brief (catalog-only) | Already fast | Data remains in existing tuples until `VACUUM FULL` |
| `ALTER TABLE SET NOT NULL` | `AccessExclusiveLock` + full scan | Add `CHECK (col IS NOT NULL) NOT VALID` first, `VALIDATE CONSTRAINT`, then `SET NOT NULL` — Postgres 12+ skips the scan | Two-migration sequence required; not generated by any surveyed tool |
| `ALTER TABLE ADD CONSTRAINT FOREIGN KEY` | `AccessExclusiveLock` + full scan of referencing column | `ADD CONSTRAINT ... FOREIGN KEY ... NOT VALID`, then `VALIDATE CONSTRAINT` | Two-migration sequence; `NOT VALID` + `VALIDATE` pattern not surfaced by any surveyed tool's DDL generator |
| `ALTER COLUMN TYPE` | `AccessExclusiveLock` + full rewrite | None general; compatible type casts sometimes free (e.g., `int` → `bigint` on some Postgres versions) | Almost always unsafe for large tables; expand-contract required |
| `DROP INDEX` | `AccessExclusiveLock` brief | `DROP INDEX CONCURRENTLY` | Must run outside transaction; sea-query supports `.concurrently()` on drop (`src/backend/postgres/index.rs:91-93`) |
| `ALTER TABLE RENAME COLUMN` | `AccessExclusiveLock` brief (catalog-only) | Already fast | Application-level compatibility must be managed separately; no surveyed tool generates rename migrations |

Citations for the above table are from general Postgres documentation; no surveyed project note covers the lock matrix in detail. The closest notes are: sqlalchemy.md (covers `CONCURRENTLY` and `NOT VALID` dialect options), sea-query.md (covers `CONCURRENTLY` and explicitly notes absence of `NOT VALID`), and django.md (covers `AddIndexConcurrently` and the transaction boundary requirement).

---

## Per-system analysis

### Django

Django is the most sophisticated of the 11 systems on this topic, though still well short of a complete solution.

**`AddIndexConcurrently` and `RemoveIndexConcurrently`** (`django/contrib/postgres/operations.py:114-172`) are first-class migration operations in `django.contrib.postgres`. They:
- Set `atomic = False` at the class level
- Use `NotInTransactionMixin` to raise `NotSupportedError` if called inside a transaction
- Emit `CREATE INDEX CONCURRENTLY` / `DROP INDEX CONCURRENTLY` via the Postgres-specific schema editor

The user must manually create a migration file with `atomic = False` containing only the `AddIndexConcurrently` operation. There is no auto-split, no auto-detection of when `CONCURRENTLY` is appropriate, and no guidance on which other operations can safely share that migration. (Source: `django/contrib/postgres/operations.py:123-126`, `django/db/migrations/executor.py:254-257`. Confidence: **high**.)

**`SeparateDatabaseAndState`** (`django/db/migrations/operations/special.py:6-61`) is the expand-contract enabler at the framework level. It allows the Python model state and the database DDL to diverge temporarily: `database_operations` runs the actual DDL while `state_operations` tells Django what the state has become. This is the correct primitive for steps 2–5 of the expand-contract pattern — but it is a low-level escape hatch that requires the user to understand the pattern and author all the operations manually. No guided workflow exists.

**`RunPython`** (`operations/special.py:187-199`) is the backfill primitive. It receives `from_state.apps` (the historical frozen model registry at the point just before the migration) and `schema_editor`. A batched backfill can be implemented as a `RunPython` operation iterating over primary key ranges. Django does not generate or suggest this pattern. (Confidence: **high** on the mechanism; **low** on any documentation of the batched-backfill pattern.)

**No `SET lock_timeout` generation.** Django's schema editor does not emit `SET lock_timeout` before DDL. The Postgres-specific override at `django/db/backends/postgresql/schema.py` (not read) may add something, but no evidence was found. (Confidence: **medium**.)

**Online-safe documentation confidence: low** — the Django docs (not read in source) are known to document expand-contract patterns, but the source does not implement them.

### Alembic

**`autocommit_block()` context manager** (`alembic/runtime/migration.py:279-370`) is the escape hatch for DDL that must run outside a transaction. It unconditionally commits the preceding transaction, then sets `isolation_level="AUTOCOMMIT"` on the connection. The docstring warns that this should be combined with `transaction_per_migration=True`. This is the mechanism a user needs to run `CREATE INDEX CONCURRENTLY` within an Alembic migration. (Confidence: **high** on the mechanism; read directly.)

```python
def upgrade():
    with op.get_context().autocommit_block():
        op.execute("CREATE INDEX CONCURRENTLY ...")
```

**`op.create_index` does not have a `postgresql_concurrently` parameter** in the autogenerate output — users must call `op.execute()` with raw SQL for `CONCURRENTLY`. There is no typed `create_index(..., concurrently=True)` in Alembic's operation API that also enforces the transaction context requirement. (Confidence: **medium** — the Alembic operation classes in `operations/ops.py` were not read exhaustively for this flag.)

**Batch operations** (`operations/batch.py`) implement copy-modify-swap for SQLite only. The comment in the project note explicitly draws the conceptual parallel to `pg_repack` (`alembic.md` — Alembic note, `operations/batch.py:442-481`): "This is conceptually the same as the `pg_repack` / `gh-ost` online migration pattern — create a shadow table, copy data, swap names. Djogi users doing online-safe column type changes on Postgres could follow this pattern manually. Alembic automates it for SQLite only."

**No expand-contract automation, no batched backfill helpers, no `NOT VALID` operators.** The project note explicitly records: "Data migration companions: Not a built-in Alembic concept." (alembic.md, `alembic/runtime/migration.py:279-370`.)

### Flyway

Flyway has the most precise handling of non-transactional DDL detection of any surveyed system, but provides zero guidance on online-safe patterns.

**Per-statement regex detection in `PostgreSQLParser`** (`flyway-database/flyway-database-postgresql/src/main/java/org/flywaydb/database/postgresql/PostgreSQLParser.java:114-137`):

```java
private static final Pattern CREATE_INDEX_CONCURRENTLY_REGEX =
    Pattern.compile("^(CREATE|DROP)( UNIQUE)? INDEX CONCURRENTLY");
private static final Pattern ALTER_TYPE_ADD_VALUE_REGEX =
    Pattern.compile("^ALTER TYPE( .*)? ADD VALUE");
```

If any statement in a script matches, the whole script is flagged `canExecuteInTransaction=false`. Flyway then correctly runs that script outside a transaction, without any user annotation. This is the only system in the survey that auto-detects non-transactional DDL needs. (Source: `PostgreSQLParser.java:114-137`. Confidence: **high**.)

Note: Flyway detects the need to not-wrap in a transaction; it does not generate `CONCURRENTLY`. The user writes `CREATE INDEX CONCURRENTLY` in their SQL file; Flyway ensures it is not wrapped. This is purely a correctness feature, not an online-safety guidance feature.

**The mixed-migration guard** (`DbMigrate.java:323-332`): Flyway throws a `FlywayMigrateException` if a migration group contains both transactional and non-transactional migrations and `mixed=false` (the default). This prevents the silent failure mode of mixing `CONCURRENTLY` statements with regular DDL in the same script. (Source: `DbMigrate.java:323-332`. Confidence: **high**.)

**No expand-contract, no batched backfill, no `NOT VALID`.** The project note's online-safe section is brief: "Nothing structural. The only source-level concession to online-safe DDL is the non-transactional auto-detection." (flyway.md, `PostgreSQLParser.java:114-137`. Confidence: **high** for source; **low** for docs.)

### Prisma

The Prisma TypeScript interface has no online-safe support — confirmed by exhaustive grep (`prisma.md`). The only operator affordance is `--create-only`: generate the naive DDL, let the user hand-edit, then apply (`packages/migrate/src/commands/MigrateDev.ts:281-287`). No warnings about locking, no expand-contract, no `NOT VALID`. (Confidence: **high** — verified absence.)

The Prisma Rust engine (examined via the `prisma-engines-reference` clone) takes a more sophisticated approach. From `flavour/postgres/connector/native/mod.rs:146-156`:

```rust
// We split the script into statements rather than submitting it all at once.
// The reason is that the Postgres simple protocol automatically wraps the script
// in a transaction, which is sometimes undesirable (e.g. when the script contains
// statements that cannot be run inside a transaction like CREATE INDEX CONCURRENTLY).
for stmt in split_script_into_statements(script) {
    match client.simple_query(stmt).await { ... }
}
```

**Each statement runs in its own auto-commit transaction.** This means a Prisma migration that contains `CREATE INDEX CONCURRENTLY` alongside other DDL will work correctly — the `CONCURRENTLY` statement runs outside a transaction while surrounding statements each get their own transaction boundary. This is the most automated non-transactional DDL handling in the survey. (Source: `prisma-engines-reference` clone, `flavour/postgres/connector/native/mod.rs:150-157`. Confidence: **high**.)

The downside is that there is no per-migration transaction wrapper at all on Postgres. This means partial-apply is possible: if statement 3 of 5 fails, statements 1 and 2 have already committed. The `applied_steps_count` column in `_prisma_migrations` tracks how far the migration got, but the engine does not resume from that step — it is audit information only (`sql_migration_persistence.rs:101-116`). (Confidence: **high**.)

The advisory lock for Prisma Migrate on Postgres is `SELECT pg_advisory_lock(72707369)` (`flavour/postgres.rs:363-389`). Djogi must use a different key to avoid conflicts with Prisma on shared databases.

### Liquibase

No online-safe support of any kind. The project note's full finding: "A user who wants online-safe DDL today writes multiple small changesets with `runInTransaction="false"` where needed, and manages their own staging via changelog files. Liquibase's posture is 'you asked us to run this SQL, we ran it'." (liquibase.md, `StandardLockService.java:153-156`. Confidence: **high**.)

No detection of `CONCURRENTLY` patterns. No documented multi-step guidance. The `runInTransaction="false"` per-changeset flag (`ChangeSet.java:752,868-870,925-931`) is the sole affordance. (Confidence: **high**.)

### Diesel

No online-safe support. The escape hatch is `run_in_transaction = false` in `metadata.toml` per migration (`diesel_migrations/src/file_based_migrations.rs:82-93`). The project note explicitly lists creating an index on an existing column as a documented example requiring `run_in_transaction = false`. No documentation of expand-contract, batched backfill, or `NOT VALID` patterns. (Confidence: **high** — verified absence.)

### SeaORM

No online-safe support. The escape hatch is `use_transaction() -> Some(false)` on a migration struct. A user can then call `manager.execute(Statement::from_string("CREATE INDEX CONCURRENTLY ..."))`. No framework guidance or examples exist for this pattern. (Source: `sea-orm-migration/src/lib.rs:40-43`, `exec.rs:184-189`. Confidence: **high** — verified absence.)

### sea-query

sea-query is a DDL query builder, not a migration runner. It does not control transaction boundaries. However, it is the only system in the survey that exposes a first-class typed API for `CREATE INDEX CONCURRENTLY`:

```rust
Index::create()
    .concurrently()
    .name("idx_name")
    .table(Table::table())
    .col(Column::column())
```

Emits: `CREATE INDEX CONCURRENTLY "idx_name" ON "table" ("col")` (`src/backend/postgres/index.rs:42-44`, `src/index/create.rs:297-300`). `DROP INDEX CONCURRENTLY` is similarly supported (`src/backend/postgres/index.rs:91-93`). (Confidence: **high**.)

**Gap:** `NOT VALID` / `VALIDATE CONSTRAINT` two-phase constraint addition is not supported. `TableAlterOption` has no `NotValidConstraint` or `ValidateConstraint` variant (`src/table/alter.rs:56-64`). (Confidence: **high**.)

**Critical caveat from the project note:** "sea-query emits the string — it does not enforce that `CREATE INDEX CONCURRENTLY` is run outside a transaction. That safety constraint is entirely the consumer's responsibility." If Djogi uses sea-query for DDL generation, Djogi's runner must enforce the transaction boundary constraint.

### SQLAlchemy

SQLAlchemy (the DDL layer, used by Alembic) supports `CREATE INDEX CONCURRENTLY` via `postgresql_concurrently=True`:

```python
Index("idx_name", col, postgresql_concurrently=True)
```

Emits `CREATE INDEX CONCURRENTLY ...` via `visit_create_index` (`lib/sqlalchemy/dialects/postgresql/base.py:2675-2764`). Same flag for `DROP INDEX CONCURRENTLY` (`lib/sqlalchemy/dialects/postgresql/base.py:2783-2786`). (Confidence: **high**.)

SQLAlchemy also supports `postgresql_not_valid=True` on constraints (`lib/sqlalchemy/dialects/postgresql/base.py:2559-2561`) via `_define_constraint_validity`. This emits `NOT VALID` on `ADD CONSTRAINT`. Djogi's DDL generator should support both of these flags. (Confidence: **high**.)

Neither flag is plumbed through Alembic's autogenerate pipeline — the autogenerate system does not emit `CONCURRENTLY` indexes or `NOT VALID` constraints automatically. Users must write migration code by hand to use them via `op.execute()` or direct SQLAlchemy DDL objects.

### refinery

No online-safe support. There is no annotation, config flag, or auto-detection for `CREATE INDEX CONCURRENTLY` or other non-transactional DDL. Running `CREATE INDEX CONCURRENTLY` inside refinery's auto-transaction will fail at the Postgres level with `ERROR: CREATE INDEX CONCURRENTLY cannot run inside a transaction block`. The project note concludes: "Users must work around this by splitting such statements into separate migrations and accepting the race risk in default (non-grouped) mode." (Source: refinery project note, confirmed by absence grep. Confidence: **high**.)

The absence of any transaction opt-out mechanism is a critical gap in refinery for Postgres production use.

### cot

No online-safe support. No `CONCURRENTLY` support anywhere. The `Custom` operation type (`Operation::custom(forwards).backwards(backwards).build()`) is the only escape hatch for hand-crafted SQL that could include `CREATE INDEX CONCURRENTLY`, but the framework provides no guidance or tooling. (Source: `cot/src/db/migrations.rs:654-657`. Confidence: **high**.)

---

## Tooling in the broader ecosystem

None of the 11 surveyed systems integrate with or document the following tools. They are the reference implementations for "what good looks like" and are relevant context for Djogi v0.3+ planning.

**pg-online-schema-change (pgosc)** — Postgres-specific online schema change tool. Uses a shadow-table approach: create a new table with the desired schema, set up triggers to replicate writes from the old table to the new, bulk-copy existing rows in batches, swap names. Fully online; handles `NOT NULL` column additions, column renames, type changes. Written in Ruby. Not integrated with any surveyed migration system.

**pgroll (xata.io)** — A newer Postgres online schema change tool that uses column-level views and multi-version schema exposition. Instead of the expand-contract pattern implemented at the application level, pgroll manages two versions of the schema simultaneously at the view layer. Applications running the old schema version see the old columns; the new deployment version sees the new columns. This is the most sophisticated approach in the ecosystem. Not integrated with any surveyed migration system.

**Reshape** — Rust tool for zero-downtime Postgres migrations. Similar multi-version schema approach to pgroll. Written in Rust, making it the most relevant ecosystem tool for Djogi integration. Not integrated with any surveyed migration system.

**pg_repack** — Postgres extension for `VACUUM FULL`-equivalent operations without `AccessExclusiveLock`. Rewrites the table to a new physical location using a shadow table and row-level locks only. Relevant for removing table bloat online. Not a migration runner integration point, but conceptually related to Alembic's `batch_alter_table` (which uses the same shadow-table pattern for SQLite).

**pt-online-schema-change (Percona)** / **gh-ost (GitHub)** — MySQL-specific online schema change tools. Use trigger-based or binlog-based shadow table approaches. Not relevant for Postgres. No surveyed system integrates or mentions them; Djogi should not target these.

**The absence of integration is universal.** No surveyed system has a documented story for delegating online migrations to pgroll, pgosc, or Reshape. The closest conceptual parallel is Prisma's `--create-only` flag, which lets users generate SQL and feed it to any external tool — but this is not an integration, just an escape hatch.

---

## Convergence / divergence

**Universal convergence: no built-in online-safe tooling.** All 11 systems, without exception, delegate online-safe migrations to the operator. There is zero controversy or split on this point — it is a universal gap.

**Split point 1: recognition vs. silence.** Six systems provide at least a transaction opt-out mechanism (Django, Alembic, Flyway, Diesel, SeaORM, Liquibase). Five provide nothing relevant (refinery, cot, and the partial-coverage of sea-query/SQLAlchemy which are DDL-layer tools without runners). The split roughly correlates with project maturity and Postgres focus.

**Split point 2: auto-detection vs. manual annotation.** Flyway auto-detects `CREATE INDEX CONCURRENTLY` and sets non-transactional mode by statement-level regex (`PostgreSQLParser.java:114-137`). Prisma's Rust engine auto-splits scripts into per-statement auto-commit transactions. All other systems require explicit annotation (`atomic = False`, `run_in_transaction = false`, `autocommit_block()`, etc.). Flyway and Prisma are the outliers; the rest require operator knowledge.

**Split point 3: typed DDL API for CONCURRENTLY.** sea-query and SQLAlchemy expose `CONCURRENTLY` as a typed builder flag. All other systems treat it as raw SQL that the user writes. The typed approach is strictly better for a generator like Djogi's `build.rs` differ — it means the generator can emit the flag rather than the user needing to hand-edit SQL.

**No convergence on expand-contract, NOT VALID, or batched backfill.** These patterns are mentioned in ecosystem documentation (and in the Django project note's "Online-safe / staged migration guidance" section at `django.md` lines 399-437, confidence: **low** — doc content not verified) but are not implemented in any surveyed system's tooling.

---

## Djogi implications

### v0.1: Conservative, non-blocking defaults

Djogi v0.1 does NOT target online-safe migrations. The following decisions are correct and should be codified:

**1. Emit `CREATE INDEX` (blocking) by default.** The `build.rs` differ should emit standard `CREATE INDEX "name" ON "table" ("col")` when generating indexes. The operator hand-edits to `CREATE INDEX CONCURRENTLY` for production deployments. This is the behavior of every surveyed system except Flyway (which auto-detects) and sea-query (which exposes a flag).

Rationale: Generating `CREATE INDEX CONCURRENTLY` by default would require auto-emitting the `-- djogi:no-transaction` directive, which changes the transactional safety properties of the migration in ways the operator may not expect. Conservative default is correct for v0.1.

**2. Support the `-- djogi:no-transaction` directive.** When a migration file contains `-- djogi:no-transaction` as the first line, the runner must execute the entire migration outside a transaction (not wrapping in `BEGIN`/`COMMIT`). This is the equivalent of Django's `Migration.atomic = False`, Diesel's `run_in_transaction = false`, SeaORM's `use_transaction() -> Some(false)`, and Liquibase's `runInTransaction="false"`. Every surveyed system with a migration runner supports this mechanism.

This directive is required for any migration containing `CREATE INDEX CONCURRENTLY`, `DROP INDEX CONCURRENTLY`, `ALTER TYPE ... ADD VALUE`, `VACUUM`, `REINDEX`, or `DISCARD ALL`. Djogi's runner should validate this at apply time (raise an error if `CONCURRENTLY` appears in a transactional migration, similar to Django's `NotInTransactionMixin`).

**3. Do not attempt to auto-detect non-transactional DDL in v0.1.** Flyway's per-statement regex detection (`PostgreSQLParser.java:114-137`) is useful but adds complexity. Defer to v0.2. For v0.1, trust the operator's `-- djogi:no-transaction` annotation.

**4. Document the expand-contract pattern in djogi-book.** The six-step pattern documented above, with concrete SQL for each step, should be a first-class section in the book. This is low effort (documentation only) and immediately differentiates Djogi's operator experience from systems that are completely silent on the topic.

**5. Document the `NOT VALID` + `VALIDATE CONSTRAINT` two-phase pattern.** The pattern for adding `NOT NULL` constraints and foreign keys without a full table scan should be documented as a named pattern in djogi-book. The two migrations required are:
- Migration A: `ALTER TABLE t ADD CONSTRAINT c CHECK (col IS NOT NULL) NOT VALID;`
- Migration B: `ALTER TABLE t VALIDATE CONSTRAINT c;` then `ALTER TABLE t ALTER COLUMN col SET NOT NULL;`

**6. Document `lock_timeout`.** Djogi-book should recommend that operators set `lock_timeout = '2s'` (or similar) in their `djogi.toml` or per-migration as a preamble SQL statement for any blocking DDL. No surveyed system does this automatically.

**7. Advisory lock key separation.** Djogi uses `pg_advisory_lock(x'DJOGMIGR'::bigint)`. Prisma uses `pg_advisory_lock(72707369)` (`flavour/postgres.rs:374`). Flyway uses a hash of the magic string `"Flyway"` plus the table name hash (`PostgreSQLAdvisoryLockTemplate.java:38-54`). Djogi's key must not collide with any of these. The key `x'DJOGMIGR'::bigint` converts to `5069656d6967720a` (if treated as ASCII bytes big-endian) or a numeric value; this should be verified against Prisma's `72707369` to confirm no collision. The values are different, so no conflict exists with Prisma.

### v0.3+: Online-safe mode flag

The survey reveals a clear opportunity: no system provides an "online-safe mode" that automatically emits multi-phase DDL. Djogi could be the first to do so.

**Proposed `online_safe = true` mode (v0.3+):**

When enabled in `Djogi.toml` or per-migration via directive, the generator would:

1. **Index creation:** emit `CREATE INDEX CONCURRENTLY` instead of `CREATE INDEX`, automatically add the `-- djogi:no-transaction` directive, and split the index creation into its own migration file if it appears alongside other DDL.

2. **`NOT NULL` constraint addition:** instead of `ALTER TABLE t ALTER COLUMN col SET NOT NULL`, emit the two-migration sequence with `CHECK (col IS NOT NULL) NOT VALID` followed by `VALIDATE CONSTRAINT`.

3. **Foreign key addition:** instead of `ALTER TABLE t ADD CONSTRAINT fk FOREIGN KEY ...`, emit the two-migration `NOT VALID` + `VALIDATE` sequence.

4. **Column type change:** refuse to generate and instead emit a warning with instructions for the expand-contract sequence. The generator cannot safely automate column type changes; it can only block naive single-migration approaches.

5. **Column rename:** use `#[field(renamed_from = "old_name")]` annotation, which Djogi already requires. In online-safe mode, emit `ALTER TABLE t ADD COLUMN new_name type`, then a backfill template, then the NOT-NULL sequence, then the column drop — across six migration files with comments indicating which step is which.

**Integration targets for v0.3+:**

pgroll (Reshape) is the most relevant integration target for Djogi, both being Rust projects with Postgres focus. A `djogi migrate --via pgroll` mode that delegates to pgroll for the actual schema change while Djogi continues to own the migration file format and ledger is architecturally viable.

**Detection question (open):** How should Djogi detect at generation time whether a generated migration contains non-safe DDL? Options:

1. Scan the generated SQL for `ALTER TABLE`, `CREATE INDEX`, etc. and classify by lock type. This is what Flyway does per-statement with regex; it works but requires maintaining a classification table.

2. Classify at the descriptor level, not the SQL level. When the `build.rs` differ generates an `ALTER TABLE ADD COLUMN NOT NULL` operation, it already knows the operation is potentially locking. Flag it before SQL emission.

Option 2 is the correct approach for Djogi because the differ operates on typed descriptors, not raw SQL. The classification can be exact (the differ knows if the column has a constant default, if the table has existing rows, etc.) rather than heuristic. This is strictly better than Flyway's regex approach.

---

## Open questions

1. **Should Djogi v0.1 validate `CONCURRENTLY` usage at apply time?** The correct behavior is to raise an error if a migration containing `CONCURRENTLY` is applied without the `-- djogi:no-transaction` directive. This requires the runner to scan the SQL before executing. The cost is low; the benefit is a clear error message rather than a Postgres error about `CONCURRENTLY` inside a transaction. Defer or implement in v0.1?

2. **`lock_timeout` as a first-class Djogi.toml setting?** A top-level `[migrations] lock_timeout = "2s"` config that the runner inserts before every blocking DDL statement would improve safety with zero operator effort. No surveyed system does this. It would differentiate Djogi's default safety posture.

3. **How to distinguish `NOT NULL DEFAULT constant` (safe on Postgres 11+) from `NOT NULL DEFAULT now()` (potentially unsafe)?** The differ can inspect the default expression at generation time and classify it. For constants, mark the migration as safe. For volatile expressions, warn. This requires constant-expression detection in the descriptor system — non-trivial but feasible.

4. **Backfill template emission.** When the differ detects that a non-nullable column is being added to a potentially non-empty table (based on schema state — there is no live data at `build.rs` generation time), it could emit a commented-out backfill template as a `RunSQL`-equivalent migration. The operator fills in the `WHERE` clause and batch size. This is better than silence.

5. **pgroll / Reshape integration feasibility.** pgroll's schema change model requires the application to advertise which schema version it supports during deployment. This requires coordination between Djogi's migration runner and the application deployment. The integration is non-trivial. Needs a design document before committing to v0.3+.

6. **Should `-- djogi:no-transaction` be a comment directive or a `[migration]` header?** A TOML header at the top of the migration file is more structured and parseable than a SQL comment directive. The precedent from the survey is mixed: Diesel uses `metadata.toml` (a separate file), SeaORM uses a Rust trait method, Django uses a Python class attribute, Liquibase uses XML attributes. A TOML header in the `.sql` file (before the first SQL statement) would be consistent with Djogi's SQL-first design and parseable without executing SQL.
