# Topic 09: Destructive and Lossy Classification

## Executive summary

Out of eleven surveyed migration systems, exactly one — Prisma — implements a two-bucket
destructive-change classifier that distinguishes "will definitely lose data" from "might lose
data." One other — Django — has an interactive prompt for a subset of destructive operations.
The remaining nine (Alembic, SQLAlchemy, Flyway, Liquibase, Diesel, SeaORM, sea-query,
refinery, cot) have no classifier at all: they execute whatever SQL the user or autogenerator
produces, and data loss is entirely the operator's problem.

Prisma's classifier is the strongest prior art for Djogi's planned two-bucket model. It is
implemented as two Rust enums — `UnexecutableStepCheck` and `SqlMigrationWarningCheck` — in
`schema-engine`. Each variant carries data probes (`COUNT(*)`, `COUNT(*) WHERE col IS NULL`)
run against the live production database at `evaluateDataLoss` time. An `unexecutableStep`
hard-blocks migration creation unless `--create-only` is passed for hand-editing. A `warning`
surfaces interactively under `migrate dev` and requires `--accept-data-loss` under `db push`.

The single most important finding: **no prior-art system is row-count-aware in its planning
phase for non-Prisma systems, and even Prisma's data probes are syntactic-plus-count rather
than semantically aware** (it does not inspect actual column values to decide whether an ALTER
TYPE USING expression preserves data). Classification is always syntactic-first with optional
count probes as a refinement.

Djogi should adopt Prisma's two-bucket classifier verbatim, with `--allow-data-loss` as the
override flag (matching the spec's intent, avoiding Prisma's slightly inconsistent
`--accept-data-loss` vs `--force` naming across subcommands). One operation Djogi should
reclassify relative to the naive Prisma mapping: `ADD UNIQUE CONSTRAINT` on a non-empty table
should be `warnings`, not `unexecutableSteps`, because the DB engine enforces the constraint
at DDL time and will hard-refuse with a useful error if duplicates exist — the migration is
runnable (the engine decides), but the user should be warned in advance.

---

## Comparison matrix

| System | Has classifier? | Bucketing | Gating mechanism | Override flag | Row-count-aware? |
|---|---|---|---|---|---|
| **Prisma** | Yes — full | Two buckets: `warnings` / `unexecutableSteps` | `unexecutableSteps` → hard block; `warnings` → interactive prompt or `--accept-data-loss` | `--accept-data-loss` (db push); `--create-only` escape hatch | Yes — probes `COUNT(*)` and `COUNT(*) WHERE col IS NULL` against prod DB |
| **Django** | Partial | One bucket: "might lose data" (rename-as-drop heuristic) | Interactive Y/N prompt; `--no-input` refuses | None (non-interactive = refuse) | No — syntactic only |
| **Alembic** | Effectively none | No classification | Autogen emits `DropColumnOp` / `DropTableOp` without warning | None | No |
| **SQLAlchemy** | None (builder only) | n/a | n/a | n/a | No |
| **Flyway** | None | None | `cleanDisabled` blocks `flyway clean` only — not DROP COLUMN/TABLE in user SQL | None for user SQL | No |
| **Liquibase** | None (OSS) | None found in OSS source | None for user changesets | None | No |
| **Diesel** | None | None | `--diff-schema` generates DROP COLUMN/DROP TABLE silently | None | No |
| **SeaORM** | None | None | `SchemaManager::drop_table` etc. execute unconditionally | None | No |
| **sea-query** | None (builder) | n/a | n/a | n/a | No |
| **refinery** | None | None | Raw SQL runner — user's responsibility | None | No |
| **cot** | None | None | `RemoveField` / `RemoveModel` generated without diagnostic | None | No |
| **Djogi** (planned) | Yes — full | Two buckets: `warnings` / `unexecutableSteps` | `unexecutableSteps` → hard block in `makemigrations` and `migrate apply`; `warnings` → surface in output and CI check | `--allow-data-loss` | Partial — count probes at evaluateDataLoss time (deferred) |

---

## The data-loss problem

A migration system that silently applies destructive operations in CI is a liability in
production. The failure modes are concrete:

- `DROP COLUMN` destroys every value in that column for every row in the table, instantly
 and irreversibly (absent a backup). A typo in a model struct — accidentally removing a
 field — becomes a production incident.
- `DROP TABLE` destroys all rows. Combined with cascade FKs, it can propagate to dependent
 tables.
- `ALTER COLUMN TYPE` from a wider to a narrower type (e.g., `TEXT` → `VARCHAR(50)`,
 `BIGINT` → `INTEGER`, `NUMERIC(20,6)` → `NUMERIC(10,2)`) truncates values that exceed
 the new precision. Postgres will refuse some of these without a `USING` clause; others it
 will silently truncate.
- Making a nullable column `NOT NULL` without providing a `DEFAULT` (or backfilling NULLs
 first) will fail at DDL execution time with a Postgres error — but a classifier can
 surface this earlier, before the migration file is written.
- Adding a `UNIQUE CONSTRAINT` to a table that already has duplicate values will fail at
 DDL time.

In CI pipelines, the absence of any classifier means these operations apply immediately in
the test environment and are promoted as-is to staging and production. The standard failure
scenario: engineer removes a field from a struct, `makemigrations` runs in CI, migration is
applied to production, data is gone. No prior warning was issued.

The nine systems with no classifier all depend on operator review of generated SQL before
applying to production. This is process-as-safety, not system-as-safety.

---

## Approaches

### Approach A: Two-bucket classifier (Prisma)

Prisma's classifier is the most complete of any surveyed system. It is implemented in two
Rust enums inside `prisma-engines` and gates the entire `migrate dev` flow.

The TypeScript API surface at `packages/migrate/src/types.ts:285-293`:

```typescript
export interface EvaluateDataLossOutput {
 migrationSteps: number
 warnings: MigrationFeedback[]
 unexecutableSteps: MigrationFeedback[]
}
```

where `MigrationFeedback = { message: string; stepIndex: number }` (prisma.md, TS clone).

The gating logic in `packages/migrate/src/utils/handleEvaluateDataloss.ts:6-30`:
- If `unexecutableSteps.length > 0` and NOT `--create-only`: hard error, abort.
- If `unexecutableSteps.length > 0` and `--create-only`: write to console.error, continue
 (user will hand-edit the generated file).
- Warnings are prompted interactively under `migrate dev` and require `--accept-data-loss`
 under `db push`. (prisma.md lines 241-244)

**Key property:** warnings that are dismissed still produce a migration file. The
`/* Warnings:... */` block is embedded as a SQL comment at the top of the generated
`migration.sql`, making the decision traceable in git history. (prisma.md lines 248, 304)

### Approach B: Interactive prompt (Django)

Django does not have a general destructive-operation classifier. The closest mechanism is
the rename-detection prompt in `InteractiveMigrationQuestioner`:

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

If the user answers "no" (or in non-interactive mode — where the default is `False`), Django
emits `RemoveField` + `AddField` instead of `RenameField`. This is a destructive, data-losing
operation, and it is generated silently. (django.md lines 341, 339)

Django's `OperationCategory` enum (`ADDITION`, `REMOVAL`, `ALTERATION`, `SQL`, `PYTHON`,
`MIXED`) is available in `--plan` output but does not block execution. (django.md line 345)
There is no `DropColumn` warning, no `DeleteModel` warning, no `AlterField` type-change
warning. The category enum is informational only.

In non-interactive mode (`NonInteractiveMigrationQuestioner`), `ask_rename` defaults to
`False`, meaning renames are silently treated as drop+add. (django.md line 339)

The practical consequence: `makemigrations --no-input` — the mode used in CI — will silently
generate data-losing `RemoveField` operations for any field that disappears from a model.

### Approach C: Warn-but-continue (Alembic)

Alembic's autogenerate pipeline produces `DropColumnOp` and `DropTableOp` objects and
renders them as Python `op.drop_column(...)` / `op.drop_table(...)` calls without any
diagnostic. (alembic.md lines 318-319, 335)

The computed-column case is the only partial exception: Alembic detects changed
`Computed` columns and emits a warning (at the compare level) because computed columns
cannot be altered in-place. (alembic.md line 342: "detected with a warning if changed,
since they cannot be altered") But this is a narrow case and does not constitute a general
destructive-operation classifier.

Alembic's `alembic check` command (`command.py:323-378`) raises
`AutogenerateDiffsDetected` if there are unapplied model changes — but this is a "drift
exists" check, not a "this drift is destructive" check.

**Assessment:** Alembic is effectively Approach D (no classification) for DROP operations.
The computed-column warning is the only carved-out exception.

### Approach D: No classification (Flyway, Liquibase, refinery, Diesel, SeaORM, sea-query, cot)

These seven systems are raw-SQL or typed-DDL builders. The classifier does not exist:

- **Flyway:** The only destructive gate is `cleanDisabled` which blocks `flyway clean`
 (a whole-schema DROP). SQL files containing `DROP TABLE` or `DROP COLUMN` run without
 any warning. (flyway.md lines 295-303) The `PostgreSQLParser` detects non-transactional
 DDL patterns (e.g., `CREATE INDEX CONCURRENTLY`) for transaction-boundary purposes, but
 makes no classification of data loss. (flyway.md lines 139-151)
- **Liquibase:** `DropTableChange`, `DropColumnChange`, and `DropAllCommandStep` run
 without confirmation flags in the OSS source. No "this changeset is destructive, require
 --force" detection. (liquibase.md line 241) Preconditions (`<preConditions>`) can be
 used to guard changesets manually, but this is user-authored, not system-enforced.
- **Diesel:** `--diff-schema` generates `DROP COLUMN` and `DROP TABLE IF EXISTS` without
 any warning or gate. The comment `// TODO: handle schema?` near `generate_drop_table`
 signals the absence is acknowledged but not implemented. (diesel.md line 207, 335-336)
- **SeaORM:** `SchemaManager::drop_table`, `drop_index`, `drop_foreign_key`, `drop_type`,
 `alter_table` are thin wrappers that execute whatever statement is passed with no
 classification, warning, or confirmation prompt. (sea-orm.md line 213)
- **sea-query:** Pure builder. Emits SQL strings on demand. No runner, no classifier.
 (sea-query.md lines 58-65)
- **refinery:** Raw SQL runner. The migration file is executed verbatim.
 (refinery.md lines 139-148, 262)
- **cot:** `RemoveField` and `RemoveModel` are generated silently by `make_remove_field_operation`
 with only a `print_status_msg(StatusType::Removing,...)` as output. No classifier,
 no two-bucket model, no Prisma-style unexecutable steps.
 (cot.md lines 228-230)

---

## Prisma's DestructiveChange classifier (deep dive)

**Confidence: high** — sourced from prisma-engines Rust source at commit 3c6e192.
Citation: prisma.md patch-pass section, "3. Two-bucket destructive classifier — Rust-side rules."

### `UnexecutableStepCheck` enum

```rust
// sql_destructive_change_checker/unexecutable_step_check.rs:7-13
pub(crate) enum UnexecutableStepCheck {
  AddedRequiredFieldToTable(Column),
  AddedRequiredFieldToTableWithPrismaLevelDefault(Column),
  MadeOptionalFieldRequired(Column),
  MadeScalarFieldIntoArrayField(Column),
  DropAndRecreateRequiredColumn(Column),
}
```

Rules for each variant (from `evaluate()` at `unexecutable_step_check.rs:36-139`):
- `AddedRequiredFieldToTable`: fires if `COUNT(*) > 0`. Safe only if table is empty.
- `AddedRequiredFieldToTableWithPrismaLevelDefault`: same probe — row count > 0 is
 unexecutable; the recommended fix is to add as optional first, populate, then make
 required.
- `MadeOptionalFieldRequired`: probes both `COUNT(*)` and `COUNT(*) WHERE col IS NOT NULL`.
 Fires only if null count > 0. If all values are already non-null, returns `None` — safe.
- `MadeScalarFieldIntoArrayField`: probes `COUNT(*) WHERE col IS NOT NULL`. Fires if
 non-null values exist.
- `DropAndRecreateRequiredColumn`: fires if `COUNT(*) > 0`. Catches type changes that
 require drop-and-recreate on a NOT NULL column.

### `SqlMigrationWarningCheck` enum

```rust
// sql_destructive_change_checker/warning_check.rs:7-48
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
- `NonEmptyTableDrop`: fires if `COUNT(*) > 0`. No warning if table is empty.
- `NonEmptyColumnDrop`: fires if non-null value count > 0. Safe if column is all-null.
- `RiskyCast`: fires if non-null values exist in the column being cast. Safe if empty/all-null.
- `NotCastable`: same trigger as `RiskyCast` but the cast is not mechanically supported —
 warning message differs (implies manual USING clause needed).
- `DropAndRecreateColumn` (nullable): fires if row count or non-null count > 0.
- `PrimaryKeyChange`: fires if `COUNT(*) > 0` (partial failure could leave table without PK).
- `UniqueConstraintAddition`: **always fires** — no data probe. Cannot check for duplicates
 without attempting the DDL.
- `EnumValueRemoval`: **always fires** — no data probe. Removing an ENUM value may fail at
 DDL time if the value is in use.

### Key non-obvious invariants (prisma.md patch-pass)

1. Data probes run against the **production database** at `evaluateDataLoss` time, not
  against the shadow DB. The classifier sees real data.
2. `UniqueConstraintAddition` and `EnumValueRemoval` always produce a warning regardless
  of data — no probe is possible for these operations before attempting the DDL.
3. `DropAndRecreateRequiredColumn` is `unexecutableSteps`, but `DropAndRecreateColumn`
  (nullable version) is `warnings`. The required-ness of the column determines which
  bucket. This is the key routing decision in the Postgres-specific checker
  (`postgres/destructive_change_checker.rs:40-182`).

### CLI integration

Under `migrate dev`:
- `unexecutableSteps.length > 0` without `--create-only` → hard abort, no file written.
- `warnings.length > 0` → interactive prompt (requires Y/N confirmation).
- Both buckets → their messages are embedded as `/* Warnings:... */` SQL comments in the
 generated migration file. (prisma.md line 248)

Under `db push`:
- Warnings require `--accept-data-loss`. No interactive prompt — this is the CI-friendly
 mode. (prisma.md line 244)

Under `migrate deploy` (production apply):
- No pre-flight `evaluateDataLoss` call. The `applyMigrations` command applies whatever is
 in the migration files without re-probing. The classification happens at generation time,
 not at apply time. (prisma.md patch-pass "Out-of-order policy" section)

---

## Django's dataloss detection (deep dive)

**Confidence: high** — sourced from django.md, commit 69d86004.

Django's dataloss detection is narrower than Prisma's. It covers exactly one scenario:
the rename heuristic. When `generate_removed_fields()` and `generate_added_fields()`
both fire for what might be the same field (same model, similar signature), the autodetector
calls `questioner.ask_rename(...)`.

The `InteractiveMigrationQuestioner.ask_rename` produces a Y/N prompt. The
`NonInteractiveMigrationQuestioner.ask_rename` (used with `--no-input`) returns `False`
unconditionally. (django.md lines 316-339)

For all other destructive operations — `RemoveField`, `DeleteModel`, `AlterField` with
narrowing type change — there is no prompt, no warning, no gating. The operations are
generated silently. The `OperationCategory` enum (`ADDITION`, `REMOVAL`, `ALTERATION`,
etc.) is printed in `--plan` output but has no mechanical effect on generation or
application. (django.md lines 344-345)

In summary:
- Django prompts for: rename (field, model) — only.
- Django does not prompt for: DROP COLUMN, DROP TABLE, ALTER COLUMN TYPE (narrowing),
 MAKE COLUMN NOT NULL with existing NULLs.
- Django in `--no-input` mode: treats renames as drop+add silently.

The `elidable` attribute on `Operation` (`operations/base.py`) controls whether the
optimizer can collapse the operation away. It is not a data-loss marker. (django.md, not
directly cited in the source for this topic but confirmed by the "Optimization pass"
section)

---

## Operation-by-operation classification

The table below covers the operations most likely to produce data loss. "Prisma bucket"
and "Django prompt" draw on the verified source material above. "Djogi recommendation"
is derived from first principles plus the Prisma source.

| Operation | Data loss? | Reversible? | Prisma bucket | Django prompt? | Djogi recommendation |
|---|---|---|---|---|---|
| `DROP COLUMN` | Yes — all values in column destroyed | No | `warnings` (`NonEmptyColumnDrop`, data-probe gated) | No | `unexecutableSteps` — stricter than Prisma; column data is never recoverable |
| `DROP TABLE` | Yes — all rows destroyed | No | `warnings` (`NonEmptyTableDrop`, data-probe gated) | No | `unexecutableSteps` — same rationale |
| `ALTER COLUMN TYPE` narrowing (e.g., TEXT → VARCHAR(50)) | Maybe — values exceeding new precision are truncated | No | `warnings` (`RiskyCast` or `NotCastable`, data-probe gated) | No | `warnings` — Postgres may refuse or truncate; warn but allow with `--allow-data-loss` |
| `ALTER COLUMN TYPE` widening (e.g., INT → BIGINT) | No | Yes | safe — no entry | No | safe |
| `MAKE COLUMN NOT NULL` with existing NULLs | "Refuse at runtime" (DB error) | — | `unexecutableSteps` (`MadeOptionalFieldRequired`, null-count probe) | No | `unexecutableSteps` — probes null count; recommend adding DEFAULT first |
| `MAKE COLUMN NOT NULL` with no NULLs | No | Yes (revert adds nullable back) | safe (null-count probe returns 0) | No | safe — count-probe at apply time |
| `ADD REQUIRED COLUMN` to non-empty table | "Refuse at runtime" (no DEFAULT) | — | `unexecutableSteps` (`AddedRequiredFieldToTable`, row-count probe) | No | `unexecutableSteps` |
| `ADD REQUIRED COLUMN` to empty table | No | Yes | safe (row-count probe returns 0) | No | safe |
| `DROP UNIQUE CONSTRAINT` | No — uniqueness guarantee is relaxed, but no values are deleted | Yes — can add back | safe — no entry | No | safe |
| `ADD UNIQUE CONSTRAINT` on non-empty table | "Refuse at runtime" if duplicates exist | — | `warnings` (`UniqueConstraintAddition`, always fires) | No | `warnings` — DB enforces at DDL time; warn unconditionally (like Prisma) |
| `DROP INDEX` | No — index is a performance artefact | Yes | safe — no entry | No | safe |
| `CREATE INDEX` | No | Yes | safe — no entry | No | safe |
| `RENAME TABLE` (explicit annotation) | No | Yes (rename back) | safe — no Prisma rename | Interactive ask | safe (Djogi uses explicit annotation, not heuristic) |
| `RENAME COLUMN` (explicit annotation) | No | Yes (rename back) | safe — no Prisma rename | Interactive ask | safe (explicit `#[field(renamed_from = "...")]`) |
| `RENAME COLUMN` without annotation (drop+add) | Yes — column values lost | No | `warnings` (`NonEmptyColumnDrop`) | Interactive Y/N | `unexecutableSteps` — Djogi's differ emits drop+add only for true removals; renamed-from annotation prevents this path |
| `DROP ENUM VALUE` | "Refuse at runtime" if value in use | No | `warnings` (`EnumValueRemoval`, always fires) | No | `warnings` — warn unconditionally; DB enforces |
| `ADD ENUM VALUE` | No | Not directly (adding back is safe; removing is warned) | safe — no entry | No | safe |
| `DROP FOREIGN KEY CONSTRAINT` | No — referential enforcement relaxed; data intact | Yes | safe — no entry | No | safe |
| `ADD FOREIGN KEY CONSTRAINT` on non-empty tables with FK violations | "Refuse at runtime" | — | `warnings` (no explicit variant; covered by general DDL failure) | No | `warnings` — warn that FK validation will run at apply time |
| `DROP PRIMARY KEY` | No data loss, but table structure fundamentally changed | No (adding back requires same column) | `warnings` (`PrimaryKeyChange`) | No | `unexecutableSteps` — Djogi models always have a HeerId/RanjId PK; this should not be generated at all |
| `TRUNCATE TABLE` | Yes — all rows destroyed | No | Not a Prisma-generated operation | No | `unexecutableSteps` — if generated, must require `--allow-data-loss` |

**Note on Djogi reclassifications vs Prisma:**

1. `DROP COLUMN` and `DROP TABLE`: Prisma puts these in `warnings` (not `unexecutableSteps`)
  because data-probe gating means "if table is empty, no warning." Djogi's recommendation
  is stricter: both go to `unexecutableSteps` regardless of row count. The rationale is
  that Djogi targets production Postgres databases where schema changes are version-controlled.
  Even a DROP COLUMN on an empty table during development could be accidentally applied to
  a staging environment with data. Requiring explicit `--allow-data-loss` for any drop is
  a conservative default that can be relaxed per operation via flag.

2. `ADD UNIQUE CONSTRAINT`: both Prisma and Djogi emit `warnings` (not `unexecutableSteps`).
  The DB engine will refuse if duplicates exist — there is nothing Djogi can do to prevent
  the failure. But because the failure is DB-enforced and the migration is syntactically
  valid, it belongs in `warnings`, not `unexecutableSteps`. The user is warned in advance
  that they should verify no duplicates exist.

---

## False positives and false negatives

### False positives (classifier flags an operation that is actually safe)

- **`DROP COLUMN` on an empty table:** Prisma's data probe correctly eliminates the warning
 (`NonEmptyColumnDrop` returns `None` if non-null count = 0). Djogi's stricter default
 (always unexecutableSteps) is a deliberate false positive — trading precision for safety.
 Operator can override with `--allow-data-loss`.

- **`ADD UNIQUE CONSTRAINT` when data is actually unique:** `UniqueConstraintAddition`
 always fires, even if all existing values are already unique. This is a false positive:
 the DDL will succeed. The Prisma rationale (prisma.md line 537): "cannot check for
 duplicates without running the migration." A SELECT-based duplicate check before issuing
 the DDL would be possible but is not implemented in any surveyed system, including Prisma.

- **`ALTER COLUMN TYPE` widening flagged by a naive classifier:** A classifier that flags
 any ALTER TYPE change without distinguishing widening from narrowing would generate false
 positives. Prisma's `RiskyCast` and `NotCastable` variants require non-null values to
 exist — an empty table generates no warning. Djogi should maintain the same data-probe
 gating.

### False negatives (operation is destructive but classifier does not fire)

- **`ALTER COLUMN TYPE` with a USING clause that preserves data:** The USING clause
 in Postgres allows specifying how to convert existing values during an ALTER TYPE
 (e.g., `ALTER COLUMN price TYPE NUMERIC(12,2) USING price::NUMERIC(12,2)`). If the
 USING expression is data-preserving, the operation is safe. But a syntactic classifier
 sees only the narrowing type change — it cannot evaluate the USING expression without
 executing it. This is a known limitation noted in Open questions below.

- **`DROP INDEX` that is the sole enforcer of a uniqueness guarantee:** If a unique
 constraint was implemented as a partial index (a Postgres-specific pattern), dropping
 that index also drops the uniqueness guarantee for affected rows. However, this does not
 lose column data — it only loses constraint enforcement. Classified as safe by both
 Prisma and Djogi's recommendation.

- **`NOT NULL` column addition with a DEFAULT:** Adding `NOT NULL DEFAULT 0` to a
 non-empty table does not lose data, but a naive classifier checking only "new required
 column on non-empty table" would fire `unexecutableSteps`. Prisma's
 `AddedRequiredFieldToTableWithPrismaLevelDefault` fires as `unexecutableSteps` even
 with a Prisma-level default — the recommendation is "add as optional, populate, make
 required." This is conservative but not technically required for a pure SQL `DEFAULT`.
 Djogi should treat a column with a SQL `DEFAULT` value as safe.

---

## Row-count awareness

**No prior-art system outside of Prisma consults row counts during migration planning.
Prisma is the only system where the classifier makes live DB queries (`COUNT(*)`,
`COUNT(*) WHERE col IS NULL`) before deciding whether to block or warn.**

All other systems that do any classification (Django's rename prompt) operate purely on
syntactic information: they inspect the diff object (what type changed from, what type
changed to; was a column removed; is a column new and NOT NULL) without touching the
database.

Prisma's row-count probes occur at `evaluateDataLoss` time — a distinct RPC call separate
from `createMigration` and `applyMigrations`. The probes run against the production database
(not the shadow DB), meaning they reflect actual production data. (prisma.md patch-pass
"Key non-obvious invariants" item 1)

### Should Djogi implement row-count-aware probes?

The Prisma experience suggests yes — the probes are cheap (`COUNT(*)` on a single column)
and substantially reduce false positives (a DROP COLUMN on an empty dev table does not
produce the same urgency as a DROP COLUMN on a table with millions of rows).

However, row-count probes add a round-trip to the production database during the planning
phase (`djogi makemigrations --plan`). This may be undesirable in environments where the
planner runs against a development schema snapshot rather than a live production database.

**Djogi recommendation: defer row-count probes to a second phase.**

Phase 1 (syntactic classification, available at `makemigrations` time without a DB
connection): flag all potentially destructive operations based on type alone.

Phase 2 (count-probe refinement, available with `--check-data` flag or automatically
when a DB connection is available): refine `unexecutableSteps` to `warnings` or `safe`
based on actual row counts. This matches the Prisma pattern without making a DB connection
mandatory for planning.

---

## CI integration patterns

### Refuse by default, flag to bypass (recommended for Djogi)

Prisma's `migrate dev` pattern: `unexecutableSteps` hard-aborts without `--create-only`.
Under `db push`, `warnings` require `--accept-data-loss`. This makes CI pipelines fail
fast when destructive operations are auto-generated, without requiring human intervention
for safe operations.

Djogi should adopt this pattern with `--allow-data-loss` as the bypass flag. The CI
workflow is:

```
djogi makemigrations --plan   # fails if unexecutableSteps detected
djogi migrate apply       # fails if unexecutableSteps in pending migrations
djogi migrate apply --allow-data-loss # applies warnings and unexecutableSteps
```

For `djogi makemigrations --plan` output, both `unexecutableSteps` and `warnings` should
be printed to stdout regardless of whether the pipeline fails, so the operator can review
what operations were classified.

### Warn by default, allow "strict mode" to fail

Alembic's `alembic check` raises on any unapplied drift (not specifically destructive
operations). A Djogi equivalent: `djogi migrate check --strict-destructive` fails if any
pending migration contains `warnings` or `unexecutableSteps`. This is additive to the
default flow.

### No classification → CI cannot gate

Flyway, Liquibase, Diesel, SeaORM, refinery, and cot all fall into this category. CI
pipelines built on these systems must rely on code review of generated SQL or external
linting tools (e.g., `squawk` for Postgres-specific DDL safety). This is process-as-safety
rather than system-as-safety.

---

## Convergence and divergence across systems

### Where systems converge

- **Universal:** No surveyed system (including Prisma) inspects actual column *values* to
 determine whether a type change is safe. Classification is always syntactic-first, with
 count probes as the only data-aware refinement.
- **Universal:** No surveyed system classifies a DROP FOREIGN KEY, DROP INDEX, or DROP
 UNIQUE CONSTRAINT as data-losing. These are structural-only changes — the column data
 is preserved.
- **Near-universal:** DROP COLUMN and DROP TABLE are treated as the most dangerous
 operations in systems that have any classification.

### Where systems diverge

- **Two-bucket vs one-bucket vs no classification:** Prisma alone has the two-bucket
 classifier. Django has a one-bucket (rename prompt only). All others have no classifier.
- **Data-probe refinement:** Prisma alone queries the production database to refine
 classification. All others classify syntactically only.
- **CI integration:** Prisma's `--accept-data-loss` / hard-abort design enables clean
 CI integration. Django's interactive prompt degrades to "refuse" in CI. All others
 offer no CI integration for destructive operations — the user must review generated SQL.
- **Embedded warning comments:** Prisma embeds `/* Warnings:... */` in the migration
 SQL file. No other system does this. It creates a permanent paper trail in git for
 any operator who later reads the migration file.

---

## Djogi implications

### Adopt Prisma's two-bucket classifier

Djogi should implement the two-bucket classifier as a first-class part of the differ and
the `djogi makemigrations` command. The two buckets:

- **`unexecutableSteps`**: operations that will definitely fail at DDL time given current
 data (NULL values prevent NOT NULL addition, existing rows prevent required-column
 addition without DEFAULT), or operations that irreversibly destroy data and should always
 require explicit `--allow-data-loss` (DROP COLUMN, DROP TABLE, column narrowing with data).

- **`warnings`**: operations that might fail at DDL time depending on current data
 (ADD UNIQUE CONSTRAINT, ADD FOREIGN KEY), or enum changes that the DB will reject if
 values are in use (DROP ENUM VALUE). These proceed unless `--allow-data-loss` is set
 in non-interactive mode, or the user confirms interactively.

The default behavior:

| Condition | Interactive (`makemigrations`) | CI / `--no-input` |
|---|---|---|
| `unexecutableSteps` present | Print, abort migration file creation | Fail with exit code 1 |
| `warnings` present | Print, prompt Y/N | Print, proceed (but surface in output) |
| `warnings` present with `--allow-data-loss` | Print, proceed | Print, proceed |
| `unexecutableSteps` with `--allow-data-loss` | Print, proceed | Print, proceed |

### Classification defaults

Djogi's initial classification table (syntactic phase, no DB probes required):

| Operation type | Default bucket |
|---|---|
| DROP COLUMN | `unexecutableSteps` |
| DROP TABLE | `unexecutableSteps` |
| ALTER COLUMN TYPE narrowing | `warnings` |
| ALTER COLUMN TYPE widening | safe |
| MAKE COLUMN NOT NULL (without count probe) | `unexecutableSteps` |
| ADD REQUIRED COLUMN to existing table | `unexecutableSteps` |
| ADD UNIQUE CONSTRAINT | `warnings` |
| ADD FOREIGN KEY CONSTRAINT | `warnings` |
| DROP UNIQUE CONSTRAINT | safe |
| DROP FOREIGN KEY CONSTRAINT | safe |
| DROP INDEX | safe |
| CREATE INDEX | safe |
| DROP ENUM VALUE | `warnings` |
| ADD ENUM VALUE | safe |
| RENAME COLUMN (via annotation) | safe |
| RENAME TABLE (via annotation) | safe |
| RENAME COLUMN (no annotation → drop+add) | `unexecutableSteps` |

### Embed warnings as SQL comments

Following Prisma's pattern (prisma.md line 248, 304), Djogi should embed the classification
result as a comment block at the top of each generated `_up.sql` file:

```sql
-- djogi:warnings
-- - You are about to drop column "old_email" on table "users". All data in the column will be lost.
-- djogi:end
```

This creates a permanent record in git of what risks the migration carries. Future operators
reading the file in git history see immediately why `--allow-data-loss` was necessary.

### Integrate with `djogi makemigrations --plan`

The `--plan` flag should display the full classification before writing any file. This is
the "dry run" mode for destructive-change detection:

```
djogi makemigrations --plan

 Migration 0042_remove_legacy_email_column

 UNEXECUTABLE STEPS:
 [1] DROP COLUMN "legacy_email" on table "users".
   All data in the column will be lost. This cannot be undone.
   To apply: djogi makemigrations --allow-data-loss

 WARNINGS:
 [2] ADD UNIQUE CONSTRAINT "users_email_uniq" on table "users".
   The migration will fail if duplicate values exist in column "email".
```

### Row-count awareness: defer

Row-count probes (querying production for `COUNT(*)`, `COUNT(*) WHERE col IS NULL`) should
be deferred to a Phase 2 enhancement. The syntactic classifier provides the essential safety
net. Count-probe refinement (which would demote some `unexecutableSteps` to safe when the
table is empty) is a usability improvement, not a correctness requirement. When added, it
should be exposed via a `--check-data` flag that performs live DB queries, not silently.

### Override semantics

`--allow-data-loss` bypasses all buckets. Djogi should never have a "no way to proceed"
state — there must always be a path forward for an operator who knows what they are doing.
The flag applies to the current invocation only; it does not persist or modify the migration
file.

---

## Open questions

1. **CAST USING and the classifier.** Postgres allows `ALTER COLUMN type TYPE bigint USING
  expression::bigint`, where the USING expression is data-preserving. A syntactic
  classifier will flag this as narrowing (or at least as a type change) even if the
  conversion is lossless. No surveyed system handles this case correctly. The recommended
  Djogi approach: classify any ALTER TYPE as `warnings` by default; if the migration file
  contains a hand-edited USING clause, the operator uses `--allow-data-loss` to
  acknowledge that they have reviewed the conversion expression. This is the `--create-only`
  pattern from Prisma applied to Djogi's context.

2. **`unexecutableSteps` for DROP COLUMN on an empty table.** Djogi's recommendation
  (always unexecutableSteps for DROP COLUMN) is stricter than Prisma's (data-probe gated).
  When count probes are implemented in Phase 2, Djogi could relax this to `warnings` for
  empty tables and `unexecutableSteps` for non-empty tables, matching Prisma's behaviour
  exactly. The Phase 1 conservative default is the right starting point.

3. **RENAME without annotation: `unexecutableSteps` or auto-detect?** Djogi's differ
  emits drop+add for any field that disappears without a `#[field(renamed_from = "...")]`
  annotation. This means a field rename without the annotation generates an
  `unexecutableSteps` DROP COLUMN. The operator is forced to either add the annotation or
  explicitly use `--allow-data-loss`. This is the correct conservative default.
  A future option: a `djogi makemigrations --detect-renames` flag that applies a
  signature-comparison heuristic (like Django's) and asks interactively — but only in
  interactive mode, never in CI.

4. **Interaction with multi-step migrations.** When a single migration file contains both
  safe and destructive operations (e.g., ADD COLUMN followed by DROP COLUMN), the
  `unexecutableSteps` for the DROP COLUMN should gate the entire migration, not just
  the specific step. The `stepIndex` field in Prisma's `MigrationFeedback` (prisma.md
  `types.ts:76-79`) allows per-step reporting while still gating the whole file.
  Djogi should adopt the same pattern: report which step is the blocker, but gate the
  entire migration file.

5. **Classification of `ALTER TABLE... RENAME TO` (table rename).** Djogi has explicit
  `#[field(renamed_from = "...")]` for field renames. A corresponding annotation for
  table renames does not appear in the current spec. Without it, table renames will be
  classified as DROP TABLE + CREATE TABLE, which is `unexecutableSteps`. The spec
  should either add a table-rename annotation or document that table renames require
  manual SQL via `RunSQL`.

6. **`EnumValueRemoval` and Postgres 15+ `ALTER TYPE DROP VALUE`.** Postgres 15
  introduced `ALTER TYPE... DROP VALUE` for enum types. Previous versions had no DDL
  path for removing an enum value — the only option was to recreate the enum, which is
  destructive. A Djogi classifier targeting Postgres 15+ (which is below Djogi's
  minimum of Postgres 18) should classify `DROP ENUM VALUE` differently: warn if in use,
  safe if not in use. Since Djogi targets Postgres 18 exclusively, the safer DDL path is
  available. A count probe (`SELECT COUNT(*) WHERE col = 'removed_value'`) could make
  this classification data-aware.
