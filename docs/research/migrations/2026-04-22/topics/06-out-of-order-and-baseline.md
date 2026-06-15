# Topic 06: Out-of-Order and Baseline

## Executive summary

Out-of-order migration and baseline/fake workflows expose one of the sharpest divides in migration-system design. The divide is not merely about features — it is about the mental model of what a migration ledger *is*.

**Out-of-order:** Two developers branch off main. Each writes a migration. The branch merged first has its migration applied (say version 0005). The branch merged second contains a migration numbered 0004 — lower than the already-applied 0005. When `migrate` runs next, should 0004 be applied? Among the 11 surveyed systems, the answers fall into three camps:

- **Reject by default** (error or ignore): Alembic, cot, refinery (with `abort_missing=true`). They treat a lower-numbered migration that is not yet in the ledger as evidence of ledger corruption, not as a legitimate work-item.
- **Allow silently** (no concept): Diesel, SeaORM, Django, Liquibase. These systems compute pending migrations as a pure set-difference and sort ascending, so the lower-numbered migration just runs — silently, without any flag in the ledger.
- **Allow with opt-in** (gated, ledger-flagged): Flyway with `outOfOrder=true`. When applied, the row sits in the ledger permanently tagged `MigrationState.OUT_OF_ORDER`.

Prisma takes a different angle: it treats a history gap as `historiesDiverge` — a structural diagnostic — rather than deciding at the runner level whether to apply or reject. The resolve path is `migrate resolve --applied`.

No surveyed system implements Djogi's planned approach: **apply by default, flag the ledger row permanently, and surface the flag in `verify`**. This is a genuine innovation grounded in CI/CD reality.

**Baseline/fake:** All systems that have any adoption story converge on a core primitive: "write a ledger row without running the SQL." But they differ sharply in *scope*:

- **Flyway `baseline`**: Writes a single floor-marker row of type `BASELINE`. Migrations with version ≤ the baseline version are permanently skipped. This is a global floor, not per-migration.
- **Alembic `stamp`**: Writes head revision identifiers directly to `alembic_version`. This is a declaration of "we are here now" — it does not distinguish skipped from applied.
- **Django `--fake` / `--fake-initial`**: Per-migration. `--fake` marks exactly the named migration(s) as applied. `--fake-initial` detects table existence and fakes the initial migration automatically.
- **Prisma `migrate resolve --applied`**: Per-migration, creates a new ledger entry with `started_at = finished_at` (no elapsed time).
- **refinery `Target::Fake`**: Per-migration, records the ledger row but executes no SQL.
- **Liquibase `changelog-sync`**: Bulk-marks all unran changesets as `MARK_RAN` without any schema validation.

These are genuinely different semantics and are easy to conflate. Baseline sets a floor. Stamp declares a current position. Fake marks individual migrations. The distinctions matter for adoption workflows.

Djogi's planned model (first-class `baseline`, `fake`, and `out-of-order` ledger flag with a `verify` command) is the most semantically rich among the surveyed systems. This document grounds those choices against all 11 prior-art systems.

---

## Comparison matrix: out-of-order

| System | Default policy | Opt-in to allow | Ledger marker? | Post-hoc detection |
|---|---|---|---|---|
| **Flyway** | Ignore (stays `IGNORED` state) | `outOfOrder=true` in config | Yes — `MigrationState.OUT_OF_ORDER` permanent on the ledger row | Via `MigrationInfoServiceImpl`; row remains flagged forever |
| **Alembic** | No concept (DAG-based; branches are explicit) | N/A — branches are first-class | No out-of-order marker; branch heads are separate `alembic_version` rows | `alembic current` / `alembic history` shows all heads |
| **Django** | Silently allowed (set-difference, no ordering check) | No flag needed — happens automatically | No marker | No detection; ledger only records applied/not-applied |
| **Prisma** | Treated as `historiesDiverge` diagnostic | `migrate resolve --applied <name>` to stamp | No explicit marker; resolved migrations get a normal `_prisma_migrations` row | `migrate status` / `diagnoseMigrationHistory` RPC |
| **Diesel** | Silently applied (pending set sorted ascending) | N/A — no concept | No marker | No detection |
| **refinery** | Error with `abort_missing=true` (default) | `set_abort_missing(false)` to log-and-continue | No marker | Log output only; ledger unchanged |
| **SeaORM** | Silently applied (set-difference) | No flag | No marker | No detection |
| **cot** | Topological sort enforces strict order | No flag | No marker | No detection |
| **Liquibase** | Silently applied (filter is `NotRanChangeSetFilter` — no ordering check) | No flag needed | No explicit marker; `ORDEREXECUTED` records true application order | `ORDEREXECUTED` vs `ID/AUTHOR/FILENAME` order divergence is observable but not flagged |
| **SQLAlchemy** | N/A — no migration runner | N/A | N/A | N/A |
| **sea-query** | N/A — no migration runner | N/A | N/A | N/A |

---

## Comparison matrix: baseline / fake / stamp

| System | Command | Semantics | Ledger effect | Preserves history? |
|---|---|---|---|---|
| **Flyway** `baseline` | `flyway baseline` (CLI) / `DbBaseline.baseline()` | Sets a floor: all migrations with version ≤ baseline version are permanently skipped on future runs | Inserts one row: `installed_rank=1, type='BASELINE', version=N, success=TRUE, checksum=NULL` | Yes — the BASELINE row is a permanent marker; nothing is deleted |
| **Flyway** skip-executing | `skipExecutingMigrations=true` (config) | Inserts history rows without running SQL — closest to per-migration fake | Normal `SUCCESS` row but body was not run | Yes — row inserted |
| **Alembic** `stamp` | `alembic stamp <revision>` | Declares "the database is currently at revision X"; no migrations run | Writes `version_num` directly to `alembic_version`; `--purge` deletes all rows first | `stamp` alone preserves existing rows; `stamp --purge` destroys history |
| **Django** `--fake` | `python manage.py migrate --fake <app> <migration>` | Marks one specific migration as applied without running it | Inserts a normal `django_migrations` row | Yes |
| **Django** `--fake-initial` | `python manage.py migrate --fake-initial` | Detects table existence via `detect_soft_applied()`; fakes the initial migration automatically if tables exist | Inserts normal rows for detected migrations | Yes |
| **Prisma** `resolve --applied` | `prisma migrate resolve --applied <migration-name>` | Marks a specific named migration as applied; creates a new ledger row with `started_at = finished_at` | `markMigrationApplied` RPC: if migration already exists in failed state, marks it rolled-back then inserts fresh row; if absent, inserts directly | Yes — old failed row stays as audit trail |
| **refinery** `Target::Fake` | `Runner::new(&migrations).set_target(Target::Fake)` | Records all pending migrations as applied without executing SQL | Inserts normal ledger rows | Yes; note: `Report.applied_migrations()` returns empty even for faked runs (surprise — see refinery note) |
| **refinery** `Target::FakeVersion(v)` | `Runner::new(&migrations).set_target(Target::FakeVersion(v))` | Records all pending migrations up to version `v` as applied without executing SQL | Inserts normal ledger rows up to `v` | Yes |
| **Liquibase** `changelog-sync` | `liquibase changelog-sync` | Writes `EXECTYPE='MARK_RAN'` for every un-ran changeset; no schema validation | `INSERT INTO DATABASECHANGELOG... EXECTYPE='MARK_RAN'` per changeset via `ChangeLogSyncVisitor` | Yes — `MARK_RAN` rows are permanent |
| **Liquibase** `changelog-sync-to-tag` | `liquibase changelog-sync-to-tag <tag>` | Same as `changelog-sync` but stops at a tagged changeset | Same as above, up to the tag | Yes |
| **Diesel** | None | No baseline/stamp/fake — manual INSERT required | N/A | N/A |
| **SeaORM** | None | No baseline/stamp/fake | N/A | N/A |
| **cot** | None | No baseline/stamp/fake | N/A | N/A |
| **SQLAlchemy** | N/A | N/A | N/A | N/A |
| **sea-query** | N/A | N/A | N/A | N/A |

---

## The out-of-order problem

### Scenario

Two developers branch off `main`. Developer A writes `0005_add_audit_table_up.sql`. Developer B, working on a parallel feature branch, writes `0004_add_user_settings_up.sql`. Developer A's PR merges first and is deployed. CI/CD applies migration 0005. Developer B's PR merges a week later. When CI/CD runs `migrate`, the pending set includes migration 0004 — which is *lower-versioned* than the already-applied 0005.

Do we apply 0004?

Arguments **for** applying:
- The migration was authored by a legitimate developer and reviewed in the PR.
- Refusing means the feature that depends on the `user_settings` table will never work.
- In most cases the two migrations are independent (different tables, different columns), so applying 0004 after 0005 is safe.

Arguments **against** applying (or against applying silently):
- Migration 0005 may have referenced a table or column that 0004 was supposed to create — a dependency inversion that silently produces broken state.
- Applying out-of-order migrations breaks the reproducibility guarantee: a fresh database created from scratch by applying 0001..0010 in order will end up in the same final state, but the execution order of 0004 and 0005 differs between fresh and upgraded instances.
- Teams that assume sequential ordering (e.g., FK dependencies, index creation ordering) will be surprised.

### Subtle failure mode: dependency ordering

Suppose migration 0004 creates the `user_settings` table with a FK into `users`, and migration 0005 adds a `settings_id` FK column on `users` pointing back to `user_settings` (a circular reference resolved in separate migrations). If 0005 is applied first, the `user_settings` table does not yet exist, so the FK will fail. This is the argument for error-on-out-of-order: the migrations may not be independent even if they appear to touch different tables.

A subtler variant: 0005 references column `user_settings.theme_preference`, which 0004 creates. If 0005 succeeds (because the column isn't referenced at DDL time, only at query time), the database is silently in a state where a runtime query will fail. No migration error, no ledger warning — invisible breakage.

---

## Out-of-order approaches

### Approach A: Reject by default

**Systems: refinery (default), cot (implicit via topological sort)**

refinery's `verify_migrations` (`refinery_core/src/traits/mod.rs:14-93`) checks every applied migration on disk against the ledger and every disk migration against the ledger's high-water mark. If a migration with version ≤ current DB version is found on disk but not in the ledger, the behavior depends on `abort_missing`:

- `abort_missing = true` (default): hard error. The migration is considered a ledger corruption.
- `abort_missing = false`: logs an error and continues.

There is no per-migration opt-in. The flag is global.

cot enforces strict ordering via the topological sorter (`cot/src/db/migrations/sorter.rs:55-61`). Migration ordering is determined by declared `DEPENDENCIES` constants. A migration with a lower number that was missed would show as pending and be applied next run — cot does not detect the out-of-order case explicitly, but its topological sort prevents it from *re-ordering* migrations.

Neither refinery nor cot has a ledger marker for out-of-order applied migrations.

### Approach B: Allow silently

**Systems: Diesel, SeaORM, Django, Liquibase**

These systems compute the pending set as:

```
pending = all_migrations_on_disk - applied_migrations_in_ledger
```

They then sort ascending and apply. There is no concept of "this migration's version is lower than migrations already in the ledger." The lower-numbered pending migration will simply run next.

Diesel's `pending_migrations` (`diesel_migrations/src/migration_harness.rs:115-129`) makes this explicit:

```rust
// compute set difference, then sort ascending by version
let mut pending: Vec<_> = all.into_iter()
 .filter(|m| !applied.contains(m.name()))
 .collect();
pending.sort_by(|a, b| a.name().version().cmp(b.name().version()));
```

There is no out-of-order guard and no warning emitted. Confidence: **high** (read source).

Django's `MigrationLoader` (`django/db/migrations/loader.py:274-340`) builds the graph based on declared `dependencies`. It does not check that applied migrations are in the topological order recorded in the graph.

Liquibase's `NotRanChangeSetFilter.accepts` (`NotRanChangeSetFilter.java:18-25`) returns `true` for any changeset whose `(ID, AUTHOR, FILENAME)` triple is not in `ranChangeSets`, regardless of ordering. Changesets inserted before already-ran changesets in the changelog file will be applied on the next `update` run. The `ORDEREXECUTED` column records the true application order, so out-of-order application is visible after the fact to anyone querying the ledger — but nothing flags it at the row level.

### Approach C: Allow with opt-in and ledger flag

**System: Flyway**

Flyway's `outOfOrder` is set on `MigrationInfoServiceImpl` (`MigrationInfoServiceImpl.java:50,77`). When building context, Flyway tags a migration as `outOfOrder = true` on its attributes if the migration's version is ≤ `context.lastApplied` (`MigrationInfoServiceImpl.java:321-334`). When selecting pending migrations in `DbMigrate.migrateGroup`:

```java
// DbMigrate.java:243-246
boolean isOutOfOrder = pendingMigration.getVersion() != null
    && pendingMigration.getVersion().compareTo(currentSchemaVersion) < 0;
```

A pending migration with version less than the current max applied version is only scheduled when `configuration.isOutOfOrder()` is true; otherwise it stays `IGNORED`. When `outOfOrder=true`, Flyway emits a loud warning at `DbMigrate.java:193-197`:

> "outOfOrder mode is active. Migration of schema... may not be reproducible."

Once the migration runs, the row sits in the history with `installed_rank > max` but `version < max applied version`. The computed state is permanently `MigrationState.OUT_OF_ORDER`. The ledger is the record that this migration was applied out of order. Confidence: **high** (read `flyway-core/src/main/java/org/flywaydb/core/internal/command/DbMigrate.java`).

**Key detail:** Flyway's flag is at the *configuration* level, not per-migration. Enabling `outOfOrder=true` allows all out-of-order migrations, not just specific ones.

### Approach D: Prisma's divergence diagnostic

**System: Prisma**

Prisma models out-of-order as a specific variant of its `HistoryDiagnostic` type (`packages/migrate/src/types.ts:63-74`):

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

When a lower-numbered migration is present on disk but not in the DB while higher-numbered ones are applied, Prisma raises `historiesDiverge`. The user resolves this with `prisma migrate resolve --applied <name>` which calls the `markMigrationApplied` RPC (`packages/migrate/src/commands/MigrateResolve.ts:135-140`). This creates a new ledger row. Confidence: **high** on the diagnostic surface; **medium** on internal engine behavior.

### Approach E: Flag-and-continue (Djogi's plan)

Djogi plans to apply out-of-order migrations by default but flag the ledger row permanently. This is novel: no surveyed system combines (a) apply by default + (b) permanent ledger flag + (c) a `verify` command that warns about flagged rows.

The closest prior art is Flyway's `MigrationState.OUT_OF_ORDER` permanent flag — but Flyway requires explicit opt-in (`outOfOrder=true`) to apply at all. Djogi differs by making application the default and making the flag the safety mechanism. The rationale:

1. CI/CD pipelines in real deployments regularly merge branches in an order that differs from migration numbering order. Refusing to run is the wrong default for teams with parallel development.
2. A flag in the ledger and a `verify` command that surfaces it gives operators visibility without blocking deployment.
3. Operators who care about strict ordering can run `djogi verify` as a post-deploy gate.

The `out_of_order_applied_at` column or `status = 'out_of_order'` flag on the ledger row is the concrete mechanism. No prior-art system implements exactly this combination.

---

## Baseline approaches

### Flyway `baseline`

Command: `flyway baseline` (CLI) or programmatic `DbBaseline.baseline()`.

Source: `flyway-core/src/main/java/org/flywaydb/core/internal/command/DbBaseline.java:88-142`.

What it does: Writes a single floor-marker row into `flyway_schema_history` with `type='BASELINE'`. The DDL constant `Database.getBaselineStatement` (`flyway-core/src/main/java/org/flywaydb/core/internal/schemahistory/JdbcTableSchemaHistory.java`) inserts:

```
installed_rank=1, version=N, description='<< Flyway Baseline >>', type='BASELINE',
script='<< Flyway Baseline >>', checksum=NULL, installed_by=current_user,
installed_on=now(), execution_time=0, success=TRUE
```

Three behavioral states (`DbBaseline.java:92-142`):

1. History table does not exist → create it and insert the baseline row atomically.
2. History table exists and already has a baseline row → if version and description match, no-op; otherwise error.
3. History table exists with non-synthetic rows but no baseline → refuse. The user must drop the table or follow the rebaselining doc.

Effect on migration runs: Flyway's `MigrationInfoServiceImpl.refresh()` skips all resolved migrations with version ≤ the baseline's version. The floor is permanent.

What Flyway lacks: There is no per-migration `fake` command. `skipExecutingMigrations` (`DbMigrate.java:368-370`) is a programmatic flag that inserts history rows without running migration bodies, but it is not a CLI verb and applies globally to a run, not to a specific migration.

Confidence: **high** (read `DbBaseline.java` in full).

### Alembic `stamp`

Command: `alembic stamp <revision>` — optionally `alembic stamp head` or `alembic stamp --purge <revision>`.

Source: `alembic/command.py:732-796`, `alembic/runtime/migration.py:558-572`.

What it does: `stamp()` computes `StampStep` objects from the revision graph and passes each to `HeadMaintainer.update_to_step()`. A `StampStep` runs through the same `HeadMaintainer` machinery as a real `RevisionStep`, except `StampStep.stamp_revision()` is a no-op — no migration function is called. The net effect: `alembic_version` is updated to contain the target revision identifier(s) without running any code.

`stamp --purge` deletes all rows first (`command.py:759`, `runtime/migration.py:598-601`) — this is the baseline flow: apply `alembic stamp head` after manually bringing the database to a known state.

The `alembic_version` table only stores active head revision identifiers. It has no concept of "baseline" vs "normally applied" — both write the same kind of row. There is no `BASELINE` type column, no `faked` flag, no `out_of_order` marker. The table is a pure set of current head revision identifiers.

Confidence: **high** (read `command.py:732-796` and `runtime/migration.py:558-572`).

**Key semantic distinction from Flyway:** Alembic `stamp` is a *declaration of current position*, not a floor. After `alembic stamp rev_42`, the system behaves exactly as if rev_42 was the last migration applied — migrations before it are considered applied (will not run), migrations after it are pending. But there is no record of *which* prior migrations were faked vs. actually run. All you know is the current head.

### Django `--fake-initial` and `migrate --fake <migration>`

Commands (verbatim from source `django/core/management/commands/migrate.py:52-64`):
- `python manage.py migrate --fake` — Mark migrations as run without actually running them.
- `python manage.py migrate --fake-initial` — Detect if tables already exist and fake-apply initial migrations if so.

Source: `executor.py:241-266` (the `if not fake:` guard at line 246) and `executor.py:310-413` (`detect_soft_applied()`).

**`--fake`**: The executor calls `record_migration()` directly without calling `migration.apply()`. This is a clean per-migration fake: any migration named on the CLI is recorded as applied without its DDL running. The ledger row is indistinguishable from a normally-applied row — same `django_migrations` table, same columns. There is no `faked` flag.

**`--fake-initial`**: Calls `detect_soft_applied()` which inspects the live database for table/column existence (not constraint or index existence — `executor.py:358-413`). If found, the migration is faked. This is Django's answer to "adopt an existing database" — but it only checks `CreateModel` and `AddField` operations. Richer schema state (constraints, custom DDL) is not verified.

Confidence: **high** (read `executor.py:241-413` and `migrate.py:52-65`).

**Key semantic distinction:** Django `--fake` is per-migration, not a floor. Each migration named in the command gets a row. Migrations not named are unaffected. This is the most granular fake among all surveyed systems.

### Prisma `migrate resolve --applied`

Command: `prisma migrate resolve --applied <migration-name>`

Source: `packages/migrate/src/commands/MigrateResolve.ts:135-140`, `packages/migrate/src/SchemaEngine.ts:91-95`.

What it does: calls the `markMigrationApplied` RPC. Per the JSDoc at `SchemaEngine.ts:91-95`:

> "There are two possible outcomes: 1) The migration is already in the table, but in a failed state. In this case, we will mark it as rolled back, then create a new entry. 2) The migration is not in the table. We will create a new entry in the migrations table. The started_at and finished_at will be the same."

The key diagnostic: `started_at = finished_at` identifies a resolved/stamped migration vs. one that actually ran (where elapsed time would differ). This is a light marker — not a dedicated status column, but observable from the timing data.

The baseline workflow with Prisma (documented in `packages/migrate/src/__tests__/Baseline.test.ts:23-80`):

1. `prisma db pull` — introspect the existing DB into PSL.
2. `prisma migrate dev --create-only` — create a migration file without applying it.
3. `prisma migrate resolve --applied <name>` — stamp it as already-applied.

Confidence: **high** on the RPC contract and the baseline workflow; **medium** on engine internals.

### refinery `Target::Fake`

Source: `refinery_core/src/runner.rs:44-50`.

```rust
pub enum Target {
  Latest,
  Version(u32),
  Fake,
  FakeVersion(u32),
}
```

`Target::Fake` records all pending migrations as applied without executing SQL. `Target::FakeVersion(v)` records up to version `v`.

Surprise (noted in the refinery project note): `Target::Fake` does not populate `Report.applied_migrations()` — the returned `Report` has an empty applied list even though ledger rows were inserted (`traits/sync.rs:47-51`). This is a bug/design flaw — callers trying to log what was "faked" get no feedback.

The ledger rows written by `Target::Fake` are indistinguishable from normally-applied rows. There is no `faked` flag in `refinery_schema_history`. Confidence: **high** (read `runner.rs:44-50` and `traits/sync.rs:47-51`).

### cot, Diesel, SeaORM, SeaQuery, SQLAlchemy — presence/absence

| System | Baseline/fake/stamp | Notes |
|---|---|---|
| **cot** | None | CLI has only `list`, `make`, `new`. No `fake`, `baseline`, `stamp`. Source: `cot-cli/src/args.rs:46-53`. |
| **Diesel** | None | `MigrationCommand` enum has six variants (`Run`, `Revert`, `Redo`, `List`, `Pending`, `Generate`) — none are repair-related. Source: `diesel_cli/src/migrations/mod.rs:34-192`. To stamp, users must manually INSERT rows into `__diesel_schema_migrations`. |
| **SeaORM** | None | `MigrateSubcommands` enum at `sea-orm-cli/src/cli.rs:109-163` has no `fake`, `baseline`, or `stamp` variant. |
| **sea-query** | N/A | DDL builder only, no runner. |
| **SQLAlchemy** | N/A | Schema metadata library only; migration runner is Alembic. |
| **Liquibase** | `changelog-sync` and `changelog-sync-to-tag` (bulk), plus `MARK_RAN` via preconditions (per-changeset). | The most semantically rich baseline story after Flyway. `EXECTYPE='MARK_RAN'` is a first-class value, distinct from `EXECUTED`. |

---

## Semantic differences

This section exists to prevent conflation of three genuinely different primitives.

### Baseline-then-migrate-forward

**Flyway's `baseline` command** is a floor declaration. After `flyway baseline --baselineVersion=N`:

- Migrations 1..N are permanently ignored on all future `migrate` runs. Flyway will never try to apply them.
- Migrations N+1, N+2,... run normally on the next `migrate`.
- The floor is encoded in a `type='BASELINE'` row in `flyway_schema_history`.

This is the "adopt an existing production database" workflow. The DBA has manually brought the DB to the state that migration N would produce. Flyway is told: "start managing from here, never go back."

The floor is **irreversible** without dropping the history table. `DbBaseline.java:107-112` explicitly refuses to write a new baseline if one already exists.

### Stamp-to-latest

**Alembic's `alembic stamp head`** declares that the database is currently at the latest revision. No migrations run. The `alembic_version` table is set to contain the head revision identifier.

This differs from Flyway `baseline` in two ways:

1. **No floor.** Stamping to `head` does not prevent earlier migrations from being noticed — if you later add a new migration between two existing ones (which Alembic handles via branches), the DAG will include it as pending. There is no "skip everything before N" semantic.
2. **No type distinction.** The stamped row in `alembic_version` is identical to a normally-applied row. You cannot tell from the ledger alone whether the migration ran or was stamped.
3. **`--purge` variant changes semantics.** `alembic stamp --purge rev` deletes all existing rows and inserts just the target revision. This is effectively "reset history" — dangerous if used casually.

### Fake-individual-migration

**Django's `--fake`** marks one specific migration as applied. It is the most granular of the three semantics. You can fake migration 0007 without affecting migrations 0001..0006 or 0008..N.

Django also has the `SEPARATE_DATABASE_AND_STATE` operation (`SeparateDatabaseAndState` in `operations/special.py:6-61`) which is related but distinct: it allows the Python state to advance without running the database DDL, or vice versa. This is the escape hatch for migrations that must be written differently from what the autodetector generates.

### Liquibase `MARK_RAN` as a first-class EXECTYPE

Liquibase's `changelog-sync` writes `EXECTYPE='MARK_RAN'` — a dedicated execution type that is distinct from `EXECUTED` (`RERAN`). The `DATABASECHANGELOG.EXECTYPE` column stores these values. When a changeset has `EXECTYPE='MARK_RAN'`, Liquibase knows it was deliberately skipped during adoption, not that it actually ran. This is the only surveyed system (besides Djogi's plan) where the ledger contains a dedicated field distinguishing "ran" from "skipped/faked."

Source: `MarkChangeSetRanGenerator.java:52-54` (for `FAILED`/`SKIPPED` — not written) and `ChangeLogSyncVisitor.java:39-58` (for `MARK_RAN`).

---

## Adoption workflows

### Greenfield: no existing DB

All 11 systems handle this identically: run all migrations from the first. The ledger is empty; all migrations are pending; apply in version order. No baseline/stamp needed.

### Existing DB with known schema version

This is the classic "we have been running manual SQL changes in production and now want to put a migration tool in charge" scenario.

The adoption workflow per system:

**Flyway:** `flyway baseline --baselineVersion=<current_schema_version>`. This inserts the floor marker. Any migration with version ≤ the baseline version will never run. Going forward, new migrations numbered above the baseline run normally. Source: `DbBaseline.java:88-142`. Confidence: **high**.

**Alembic:** `alembic stamp <current_revision>`. The database is declared to be at the target revision. No floor is set — if earlier revisions are added to the DAG later (via branches), they would show as pending. For linear histories this is not an issue. Source: `command.py:732-796`. Confidence: **high**.

**Django:** `python manage.py migrate --fake <app> <migration_name>` for each migration that corresponds to already-applied SQL, or `--fake-initial` if the initial migration created the tables that already exist. The `detect_soft_applied()` mechanism checks table/column existence. Source: `executor.py:241-413`. Confidence: **high**.

**Prisma:** Three-step workflow: `prisma db pull` → `prisma migrate dev --create-only` → `prisma migrate resolve --applied <name>`. Source: `Baseline.test.ts:23-80`. Confidence: **high**.

**Diesel:** No CLI support. Must manually `INSERT INTO __diesel_schema_migrations (version, run_on) VALUES ('...', NOW())` for each past migration. Source: `MigrationCommand` enum at `diesel_cli/src/migrations/mod.rs:34-192` — no stamp/fake variants. Confidence: **high** (absence confirmed).

**refinery:** `Runner::new(&migrations).set_target(Target::Fake)` or `Target::FakeVersion(v)`. Source: `runner.rs:44-50`. Confidence: **high**.

**SeaORM:** No support — no `fake`/`baseline` subcommand. Source: `sea-orm-cli/src/cli.rs:109-163`. Manual INSERT required. Confidence: **high** (absence confirmed).

**cot:** No support. Source: `cot-cli/src/args.rs:46-53`. Confidence: **high** (absence confirmed).

**Liquibase:** `liquibase changelog-sync` (all changesets) or `liquibase changelog-sync-to-tag <tag>` (up to a tag). Source: `ChangelogSyncCommandStep.java:55-84`. Confidence: **high**.

### Existing DB with unknown schema version

This is the hardest case. The DBA does not know which migrations have been applied. No migration tool can safely infer this without examining the live schema.

**Which systems attempt this?**

- **Liquibase** has `generateChangeLog` / `GenerateChangelogCommandStep` which introspects a live DB and generates a changelog from it. Source: `GenerateChangelogCommandStep.java` (referenced in the Liquibase note; not read in full). This is the closest prior art to "adopt from an unknown schema." The generated changelog, when combined with `changelog-sync`, effectively creates a baseline from scratch.

- **Prisma** has `prisma db pull` which introspects a live DB into PSL. This is not migration adoption per se — it generates a schema file — but combined with `migrate dev --create-only` and `migrate resolve --applied`, it enables adoption. The key insight from `MigrateStatus.ts:170-198` is that Prisma detects "no `_prisma_migrations` table" and suggests the baseline flow.

- **Django** has `manage.py inspectdb` which introspects a live DB and emits Python model code. This is not migration-aware — it generates models, not migrations. `--fake-initial` with `detect_soft_applied()` (`executor.py:310-413`) checks table and column existence for the initial migration only — but it does not handle complex histories.

- **Alembic** does not attempt this. `alembic stamp` requires the user to know which revision to stamp to.

- **Flyway**, **Diesel**, **refinery**, **SeaORM**, **cot** do not attempt this. Manual analysis required.

No system handles this fully automatically. Liquibase's `generateChangeLog` is the closest prior art — it can generate a changelog that describes the current DB state, which can then be synced via `changelog-sync`. Djogi's planned `djogi adopt --infer` command for a future version would follow this pattern.

---

## Convergence / divergence

### Convergence

All systems that claim to support production adoption have at least one of:
- A `stamp`/`fake`/`baseline` primitive that writes ledger rows without running SQL.
- A detection mechanism that checks table/column existence and auto-fakes.

The primitive (write a row without running SQL) is universal to any system that supports adoption.

### Divergence

**Out-of-order default**: Systems split sharply into "apply silently" (Diesel, SeaORM, Django, Liquibase) vs. "error by default" (refinery) vs. "allow with opt-in" (Flyway). No common default.

**Baseline semantics**: Flyway's floor model (skip all versions ≤ N forever) vs. Alembic's declaration model (I am now at revision X) vs. Django's per-migration model (this specific migration was applied) are genuinely different. None is a strict superset of another.

**Ledger fidelity on fake**: Liquibase (via `MARK_RAN`) and Flyway (via `BASELINE` type) preserve the distinction between "ran" and "faked" in the ledger. Alembic, Django, refinery, Prisma do not — faked rows are indistinguishable from applied rows in the ledger schema.

---

## Djogi implications

### Out-of-order: apply by default, flag the ledger

**Recommendation:** Apply out-of-order migrations without requiring a flag. Record the application in the ledger with a dedicated `out_of_order` boolean column (or `status = 'out_of_order'`). Emit a warning to stderr at apply time. Surface out-of-order rows in `djogi verify`.

**Rationale:** CI/CD realities. Teams with parallel development will regularly produce out-of-order version numbers. Blocking the deploy is worse than applying with a warning. The `verify` command gives teams who care about strict ordering a CI gate.

**Prior art gap:** No surveyed system does this. Flyway is the closest (permanent `OUT_OF_ORDER` state on the ledger row, `outOfOrder=true` opt-in to apply). Djogi's innovation: make apply the default, make the flag the safety mechanism.

**Open question:** How does out-of-order interact with `down`? If migration 0005 is applied, then 0004 is applied out-of-order, and the operator runs `djogi migrate down 1`, what is the "latest applied" from which to roll back? Options:

1. Roll back the most recently applied by `installed_rank` (0004, which has a higher `installed_rank` than 0005 because it was applied later). This is correct for the ledger's temporal ordering.
2. Roll back by version number (0005, which is higher-numbered). This is what a naive "roll back the latest version" implementation would do — and it is wrong.

The answer must be: roll back by `installed_rank` descending (temporal order of application), not by version number. This must be explicit in the spec. No surveyed system documents this edge case explicitly.

### Baseline: first-class command, dedicated `status`

**Recommendation:** `djogi migrate baseline --version N` writes a ledger row with `status = 'baseline'`, `version = N`, `checksum = NULL`, `applied_at = now()`. All migrations with version ≤ N are permanently skipped.

This mirrors Flyway's semantics but uses a typed `status` column instead of a `type` column with a magic string. The `status` column should be an enum: `pending`, `applied`, `faked`, `baseline`, `out_of_order`, `failed`.

**Distinguish from stamp:** Baseline sets a floor (skip everything below). Stamp declares a position (we are here). Djogi should support both:
- `djogi migrate baseline --version N` → floor
- `djogi migrate stamp --version N` → declaration of current position (for Alembic-style DAG use cases, or for single-migration declaration)

For v0.1, if Djogi is linear, `baseline` is sufficient. `stamp` can be deferred.

### Fake: per-migration, dedicated `status`

**Recommendation:** `djogi migrate fake <migration-name>` writes a ledger row with `status = 'faked'`. The checksum is computed from the migration file and stored, so drift can still be detected later if the file is changed post-fake.

This differs from all surveyed systems: faked rows have an explicit `status = 'faked'` in the ledger, making them distinguishable from normally-applied rows. Liquibase's `MARK_RAN` is the closest prior art but it is only written by `changelog-sync`, not by a per-migration command.

### Adopt-from-existing: defer to `djogi adopt --infer`

Adoption of a production database with unknown schema version requires schema introspection. This is not v0.1 scope. The workflow:

1. `djogi adopt --infer` → introspect the live DB, generate a synthetic migration that represents the current state, write it to the migrations directory.
2. `djogi migrate baseline --version <synthetic>` → mark the synthetic migration as the floor.
3. Going forward, new migrations run normally.

This is the Liquibase `generateChangeLog` + `changelog-sync` pattern, implemented in Rust against Postgres catalogs. Defer entirely.

---

## Open questions

1. **Out-of-order and `down`:** If 0005 is applied, then 0004 is applied out-of-order (with a higher `installed_rank`), and the operator rolls back 1 migration — is 0004 the target (most recent by `installed_rank`) or 0005 (highest version number)? The correct answer is 0004 (most recent by `installed_rank`), but this must be explicit in the spec. No surveyed system documents this.

2. **Out-of-order and reproducibility:** A fresh database and an upgraded database will have the same final schema if all migrations are idempotent, but the execution *order* differs (fresh: 0001..0004..0005 in order; upgraded: 0001..0005 then 0004 out-of-order). If migration 0005 depends on state created by 0004, the upgraded path will fail but the fresh path will succeed. `djogi verify` should optionally detect this by checking whether any out-of-order-flagged migration appears as a dependency of a migration that was applied before it.

3. **Baseline and down migrations:** If a baseline row exists at version N, and the operator runs `djogi migrate down` past version N, what happens? Flyway does not allow rolling back past a baseline. Djogi should define the same behavior: the baseline row is the floor and cannot be crossed going down.

4. **Fake and checksum:** Should a faked migration's checksum be the checksum of the migration file at fake-time? If the file is later edited, `djogi verify` should detect the drift even for faked migrations. This is semantically cleaner than refinery or Django, which store no checksum on faked rows.

5. **`djogi verify` output format:** The verify command needs to report: (a) out-of-order applied migrations, (b) faked migrations, (c) checksum mismatches, (d) missing migration files (applied but no longer on disk). How should these be prioritized and formatted? None of the surveyed systems has a single command that covers all four.

6. **Per-migration out-of-order opt-in:** Flyway's `outOfOrder` is a global flag. Djogi's design (per-migration pragma `-- djogi: out-of-order=allow`) would be more precise, but requires parsing the migration file header before deciding whether to apply. This is feasible and is a genuine improvement over Flyway.
