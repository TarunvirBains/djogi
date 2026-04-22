# Topic 07: Rename Handling

## Executive summary

Across all eleven surveyed systems, no tool has reliable, automatic rename detection that is
simultaneously safe for production use in CI. The landscape splits into four camps:

- **No detection at all (most systems):** Prisma, Alembic, Diesel, refinery, SeaORM, cot, Flyway,
  Liquibase's diff, and SQLAlchemy/MetaData all treat a rename as DROP + CREATE. Data loss is the
  default outcome unless the user intervenes.
- **Interactive prompt (Django only):** `makemigrations` asks the user at generation time. Correct
  in development; silently destructive in CI (`--no-input` defaults `ask_rename` to `False`).
- **Explicit annotation required (Liquibase column/table only):** `<renameColumn>` and
  `<renameTable>` change types must be hand-authored. The diff tool does not emit them.
- **No heuristic anywhere:** Not one of the eleven surveyed tools applies a name-similarity or
  column-position heuristic for rename detection in its autogenerate path. The "heuristic rename"
  approach exists in the literature but is absent from every codebase studied here.

Djogi's planned default — emit DROP + CREATE, require explicit `#[field(renamed_from = "...")]`
annotation for a true rename — is consistent with the majority of the field, avoids all false-positive
risk, and is strictly safer than Django's prompt for automated pipelines.

---

## Comparison matrix

| System | Detection | Mechanism | False-positive guard | Table renames | Column renames | Type (ENUM) renames | Index renames |
|---|---|---|---|---|---|---|---|
| **Django** | Heuristic + prompt | Signature comparison + `ask_rename` / `ask_rename_model` interactive prompt | User answers at terminal; `NonInteractiveMigrationQuestioner` defaults to `False` (destructive) | Yes — prompt | Yes — prompt | No first-class support | Yes — `RenameIndex` op, no prompt (diff-only) |
| **Alembic** | None | No rename logic in `autogenerate/compare/`; `RenameTableOp` is hand-written only | N/A — no detection to guard | No auto-detect; explicit `RenameTableOp` | No auto-detect | No auto-detect | No auto-detect |
| **Prisma** | None | DROP + CREATE emitted; explicit `@map` / `@@map` annotations affect PSL name vs DB name | N/A | No | No | No | No |
| **Liquibase** | None (diff) | `DiffToChangeLog` emits add + drop; `<renameColumn>` / `<renameTable>` are hand-written change types | N/A | No auto-detect; explicit `<renameTable>` | No auto-detect; explicit `<renameColumn>` | No | No |
| **Flyway** | None | No model; user writes SQL; rename is whatever the user writes | N/A | User-written SQL | User-written SQL | User-written SQL | User-written SQL |
| **Diesel** | None | `--diff-schema` emits `DROP COLUMN` + `ADD COLUMN` | N/A | No | No | N/A | N/A |
| **refinery** | None | No autogen; pure runner | N/A | N/A | N/A | N/A | N/A |
| **SeaORM** | None | User writes DDL explicitly | N/A | No | No | No | No |
| **cot** | None | Static AST diff emits `RemoveField` + `AddField` | N/A | No | No | N/A | No |
| **SQLAlchemy** | None | MetaData diff has no identity-mapping step | N/A | No | No | No | No |
| **Djogi (planned)** | None (default) | DROP + CREATE default; explicit `#[field(renamed_from)]` for true rename | N/A (no heuristic) | No auto-detect; explicit annotation | Explicit `#[field(renamed_from)]` | No auto-detect | No auto-detect |

**Confidence notes:** All "None" claims are high-confidence from source inspection. Django's
prompt is high-confidence from `questioner.py:223-245` and `autodetector.py:581-647`, `1048-1108`.
Alembic's absence is high-confidence from exhaustive grep of `autogenerate/compare/`. Prisma's
absence is high-confidence from the generated fixture `20201203153838_draft/migration.sql`.
Liquibase's hand-written `<renameColumn>` is high-confidence from `RenameColumnChange.java:15-22`
and `ChangedColumnChangeGenerator.java:195-201`. Index rename for Django is high-confidence from
`autodetector.py:1376-1463`.

---

## The rename problem

### Why it matters: data loss

When a column named `name` is dropped and a column named `full_name` is added, there are two
semantically distinct scenarios:

1. **True rename:** The user wants the same data, just under a new column name. The correct DDL is:
   ```sql
   ALTER TABLE users RENAME COLUMN name TO full_name;
   ```
   This is safe: no data is lost, the operation is transactional on Postgres, and it runs in
   milliseconds (only a catalog update, no table rewrite).

2. **Drop and add:** The user actually wants to remove `name` entirely and introduce a brand-new
   `full_name` column (which will start NULL or with a specified default). The correct DDL is:
   ```sql
   ALTER TABLE users DROP COLUMN name;
   ALTER TABLE users ADD COLUMN full_name TEXT;
   ```
   Every row loses the value previously stored in `name`. In a non-empty table, this is
   irreversible data loss.

A migration system that cannot distinguish these two scenarios defaults to the destructive path.
On a development database with seed data this is tolerable. On a production database with millions
of rows it is a catastrophe.

### The guessing problem

Suppose the autogenerate diff sees:
- `old_schema.users.columns` contains `name TEXT NOT NULL`
- `new_schema.users.columns` contains `full_name TEXT NOT NULL`

The diff engine must choose: is this a rename (DROP + CREATE data loss) or a rename (ALTER RENAME
no data loss)? Without additional information, it cannot know. Heuristics can guess — same type,
same nullability, same position — but they can be wrong. The false-positive scenario is:
a developer deletes column `first_name` and adds an unrelated column `full_name` for a different
purpose. A heuristic might see "one dropped, one added, types match" and emit `ALTER ... RENAME`,
silently retaining data in `full_name` that should have been empty.

### Real-world consequence

If the heuristic fires incorrectly in production:
- Old data from `first_name` silently populates the new `full_name` column.
- Application code writing fresh records sees the old data persisted as if it were the new column's
  initial data.
- There is no SQL error — the schema is consistent; the data is simply wrong.
- Rollback requires a restore from backup, not just DDL reversal.

This class of bug is worse than a failing migration: it passes silently and corrupts data.

---

## Approaches

### Approach A: No detection — default destructive (DROP + CREATE)

**Systems: Prisma, Alembic, Diesel, refinery, SeaORM, cot, Flyway, Liquibase (diff path)**

The autogenerate diff engine produces DROP + CREATE operations for any column or table whose name
does not appear in the other state. No heuristic is attempted. The user is responsible for
recognizing that their intended rename would be destructive and manually editing the generated
migration file.

**Prisma** is the clearest documentation of this choice. The fixture migration for the rename
`viewCount20 → viewCount` reads:

```sql
/*
  Warnings:

  - You are about to drop the column `viewCount20` on the `Blog` table. All the data in the column
    will be lost.
  - Added the required column `viewCount` to the `Blog` table without a default value. This is not
    possible if the table is not empty.
*/
-- RedefineTables
PRAGMA foreign_keys=OFF;
CREATE TABLE "new_Blog" (...);
INSERT INTO "new_Blog" ("id") SELECT "id" FROM "Blog";
DROP TABLE "Blog";
ALTER TABLE "new_Blog" RENAME TO "Blog";
```

(Source: `packages/migrate/src/__tests__/fixtures/existing-db-1-draft/prisma/migrations/
20201203153838_draft/migration.sql:1-19`. Note this is SQLite's copy-swap pattern; on Postgres
the engine emits `DROP COLUMN` + `ADD COLUMN` directly.)

**Alembic** is documented explicitly in its note:

> Column renames: No rename heuristic exists anywhere in `alembic/autogenerate/compare/`. A rename
> appears as `drop_column` + `add_column`. This is intentional — autogenerate does not detect
> renames. (`RenameTableOp` exists in `ops.py:1451-1485` as an explicit user-invoked operation, not
> an autogenerate output.)

(Source: `alembic.md`, confidence: high — zero rename detection logic found in `autogenerate/compare/`.)

**Diesel** confirms the same:

> The diff algorithm has no rename heuristic. A renamed column is detected as a `removed_column` +
> `added_column` pair, which will generate `DROP COLUMN` + `ADD COLUMN` SQL — a destructive,
> data-losing operation. Source: `diesel_cli/src/migrations/diff_schema.rs:244-284`.

(Source: `diesel.md`, confidence: high.)

**cot** also emits `RemoveField` + `AddField` for renames. No heuristic, no warning:

> No heuristic rename detection, no explicit annotation. A field rename produces a `RemoveField`
> followed by an `AddField` — i.e., drop-and-recreate with data loss.

(Source: `cot.md`, confidence: high.)

**Liquibase's diff path** (distinct from its hand-written change types): `DiffToChangeLog` does not
emit `RenameColumnChange` for renames between snapshots. There is no identity-mapping step. The only
appearance of `RenameColumnChange` in the diff output path is as part of a type-conversion workaround
(`ChangedColumnChangeGenerator.java:195-201`) — not semantic-rename detection.

(Source: `liquibase.md`, confidence: medium — verified the one place rename appears in the diff
output path I read; did not exhaustively scan every snapshot/diff comparator.)

**What Djogi's planned approach inherits from Approach A:**

Djogi plans to default to DROP + CREATE at the diff engine level. This is consistent with the
majority of surveyed systems, avoids all heuristic false-positive risk, and is the safer production
default. The burden of recognizing a rename shifts entirely to the developer, who uses
`#[field(renamed_from = "old_name")]` to opt into `ALTER TABLE ... RENAME COLUMN` semantics.
Djogi's explicit annotation is strictly superior to unguarded DROP + CREATE because it makes the
rename intent visible in the struct definition, not just in a hand-edited SQL file.

### Approach B: Heuristic detection

**Systems: None in production among surveyed tools.**

None of the eleven systems implement a name-similarity or column-position heuristic for rename
detection in their autogenerate path. The heuristic approach is discussed in academic contexts
and in community blog posts but is absent from every codebase studied here.

The closest thing is Django's field-signature comparison in `create_renamed_fields()`, but Django
immediately follows the comparison with an interactive prompt — it does not emit a rename operation
without human confirmation. This is not a heuristic that fires automatically; it is a heuristic that
surfaces a candidate for human review (see Approach C below).

Alembic, despite being the most sophisticated autogenerate system studied, explicitly decided not
to implement rename detection. The decision is architectural: rename detection requires an identity
claim that the diff engine cannot make safely from types and positions alone.

**Why a pure heuristic is risky:**

Consider a simple heuristic: "if exactly one column was dropped and exactly one was added in the
same table, and their types match, detect as rename."

This heuristic fires a false positive whenever a developer:
1. Drops a column of type `TEXT NOT NULL` to remove a feature.
2. Adds a different column of type `TEXT NOT NULL` for a new feature.

In a large multi-developer codebase this pattern occurs constantly (e.g., `email_override TEXT`
is dropped when the feature is removed, and `display_name TEXT` is added for the new profile page,
in the same migration window). The heuristic would silently rename `email_override` to
`display_name`, preserving old email strings as display names. The migration succeeds; the data
is wrong; there is no error to catch.

A stricter heuristic (require matching name prefix, matching position, matching type, and
matching nullable) reduces but does not eliminate false positives. No surveyed system found the
false-positive rate low enough to ship without a human in the loop.

### Approach C: Interactive prompt

**Systems: Django only.**

Django's `makemigrations` command uses field-signature comparison to identify potential rename
candidates, then prompts the developer interactively before emitting either `RenameField` or
`RemoveField` + `AddField`.

**The exact prompt text** (verbatim from source):

For field renames (`questioner.py:223-236`, Django commit `69d86004f7b3c9ed223c18998c2b799d1670474f`):

```python
def ask_rename(self, model_name, old_name, new_name, field_instance):
    """Was this field really renamed?"""
    msg = "Was %s.%s renamed to %s.%s (a %s)? [y/N]"
    return self._boolean_input(
        msg % (model_name, old_name, model_name, new_name,
               field_instance.__class__.__name__),
        False,
    )
```

Runtime output example: `Was User.name renamed to User.full_name (a CharField)? [y/N]`

For model (table) renames (`questioner.py:238-245`):

```python
def ask_rename_model(self, old_model_state, new_model_state):
    """Was this model really renamed?"""
    msg = "Was the model %s.%s renamed to %s? [y/N]"
    return self._boolean_input(
        msg % (old_model_state.app_label, old_model_state.name, new_model_state.name),
        False,
    )
```

Runtime output example: `Was the model myapp.User renamed to MyUser? [y/N]`

**What happens when the user answers "no" or when running non-interactively:**

`NonInteractiveMigrationQuestioner` overrides `ask_rename` to return `False`
(`questioner.py:67-73`). Django then emits `RemoveField` + `AddField` — the destructive path.
This means `python manage.py makemigrations --no-input` silently generates data-losing migrations
whenever a field or model is renamed.

(Source: `django.md`, confidence: high — read `questioner.py` in full.)

**What triggers the prompt — the detection algorithm:**

`create_renamed_fields()` (`autodetector.py:1048-1108`) iterates over new field keys not in old
field keys, then over old field keys not in new field keys within the same model. It calls
`deep_deconstruct()` on both field instances and compares the serialized form. If the
deconstructions match (modulo `db_column` if the old column name can be preserved), the prompt is
fired.

`generate_renamed_models()` (`autodetector.py:581-647`) compares sets of added vs. removed model
names within the same app. For each pair, it calls `only_relation_agnostic_fields()` to strip FK
target references before comparing field definitions. If the stripped field definitions match, the
model rename prompt is fired.

**A known false-positive class unique to Django:** `only_relation_agnostic_fields()` strips all
`to=` references from FK fields before comparing model definitions. This means a model with a
self-referential FK will match a model with a FK pointing to a different table — potentially
generating a false rename prompt even when the two models are structurally different except for
their FK targets. (Source: `django.md`, SURPRISE 6, `autodetector.py:113-125`.)

**CI handling:**

Any Django project running `makemigrations` in CI with `--no-input` silently generates destructive
migrations when fields are renamed. The developer must either:
- Answer the interactive prompt locally (before committing), or
- Write `RenameField` / `RenameModel` operations manually in the migration file.

There is no `--detect-renames` flag or equivalent annotation system. The CI failure mode is
invisible: the migration is generated, committed, and deployed — then fails on production data or
silently loses it.

**Index rename detection (Django, no prompt):**

Django handles index renames differently from field renames. `create_altered_indexes()`
(`autodetector.py:1376-1463`) compares old and new index objects using full `deconstruct()`
comparison excluding the name attribute. If everything matches except the name, it emits
`RenameIndex` instead of `RemoveIndex` + `AddIndex`. **No interactive prompt is used for index
renames.** This is an automatic, silent rename detection for indexes — the only case in the
surveyed tools where a rename is auto-detected and emitted without a human confirmation step.

(Source: `django.md`, confidence: high.)

### Approach D: Explicit annotation required

**Systems: Liquibase (change types), Djogi (planned, via `#[field(renamed_from)]`)**

Liquibase ships first-class `<renameColumn>` and `<renameTable>` change types. The change types
emit `ALTER TABLE ... RENAME COLUMN` and `ALTER TABLE ... RENAME TO` via `RenameColumnGenerator`
and `RenameTableGenerator`. The user must write these by hand; the diff tool never emits them.

```xml
<changeSet id="2" author="developer">
    <renameColumn tableName="users" oldColumnName="name" newColumnName="full_name" />
</changeSet>
```

(Source: `liquibase.md`, `RenameColumnChange.java:15-22`. Confidence: high.)

Djogi's planned `#[field(renamed_from = "old_name")]` annotation is structurally equivalent: the
developer explicitly declares the rename in the model definition, and the diff engine emits
`ALTER TABLE ... RENAME COLUMN` instead of DROP + ADD when it sees the annotation. Unlike
Liquibase, the annotation lives directly on the struct field — the intent is permanently visible
in the codebase rather than requiring a separate migration file lookup.

---

## Table renames specifically

No surveyed system detects table renames automatically without a human confirmation step.

- **Django:** `generate_renamed_models()` fires the `ask_rename_model` prompt. The same
  false-positive risk applies: field definitions are compared after stripping FK targets. In
  `--no-input` mode, the rename is treated as `DeleteModel` + `CreateModel` — all data in the table
  is lost. (Source: `django.md`, `autodetector.py:581-647`.)
- **Alembic:** `RenameTableOp` exists as an explicit hand-written operation only. No autogenerate
  output. (Source: `alembic.md`, `ops.py:1451-1485`.)
- **Liquibase:** `<renameTable>` change type is hand-written. The diff path emits DROP + CREATE.
  (Source: `liquibase.md`.)
- **All others:** DROP + CREATE.

Table renames are operationally more dangerous than column renames for two reasons:
1. The entire table's data is at risk, not just a single column.
2. Foreign keys referencing the table may need updating across the database.

On Postgres, `ALTER TABLE old_name RENAME TO new_name` is transactional, non-data-destructive, and
updates the system catalog atomically. All FK constraints, indexes, and sequences continue to work
after the rename without manual intervention. Losing this path via DROP + CREATE means also losing
all row data in every foreign-keyed table (unless the FK cascade is handled separately).

Djogi's `#[field(renamed_from)]` covers column renames. A `#[model(renamed_from = "old_table")]`
attribute (if added) would cover table renames. This is a natural extension of the same pattern.

---

## Type renames (Postgres ENUMs and custom types)

No surveyed system has first-class support for Postgres ENUM renames in its autogenerate path.

- **Alembic:** `autocommit_block()` is documented for `ALTER TYPE mood ADD VALUE 'soso'`
  (which cannot run inside a transaction on Postgres < 12). The example:
  ```python
  def upgrade():
      with op.get_context().autocommit_block():
          op.execute("ALTER TYPE mood ADD VALUE 'soso'")
  ```
  (Source: `alembic.md`, `runtime/migration.py:279-370`.) This handles value addition to an existing
  ENUM, not ENUM renaming.
- **sea-query:** `TypeAlterStatement` supports `ALTER TYPE ADD VALUE / RENAME`.
  (Source: `sea-query.md`, `src/backend/postgres/types.rs:66`.) This is a DDL builder, not an
  autogenerate tool.
- **All others:** No ENUM rename support in autogenerate.

Postgres supports `ALTER TYPE old_name RENAME TO new_name` (available since Postgres 9.0). The
equivalent of DROP + CREATE for an ENUM would be: remove all columns referencing the old type, drop
the type, create the new type, re-add the columns. This is significantly more destructive than a
`RENAME TO`. No autogenerate tool in the survey handles this case.

For Djogi: ENUM renames at the Postgres type level are out of scope for the initial implementation.
The recommended practice is to document that ENUM renames require hand-written migrations using
`ALTER TYPE ... RENAME TO`. If Djogi gains a type-alias abstraction for ENUMs in the future, a
`renamed_from` attribute at the type level would be the natural extension.

---

## Index renames

**Django** is the only surveyed system that auto-detects index renames without a prompt, and does
so silently:

`create_altered_indexes()` (`autodetector.py:1376-1463`) computes this: compare old and new index
sets using `deconstruct()` excluding the `name` attribute. If everything else matches, emit
`RenameIndex(model_name, old_index_name, new_index_name)` instead of `RemoveIndex` + `AddIndex`.

(Source: `django.md`, confidence: high.)

On Postgres, `ALTER INDEX old_name RENAME TO new_name` is safe, transactional, and instant. The
Django silent auto-detection for indexes is correct in the common case (rename without semantic
change). The false-positive risk is lower for indexes than for columns because:
- Index definitions are fully structural (column list, uniqueness, expression, partial predicate).
- An index with the same column list, same uniqueness, and same expression but a different name is
  almost certainly a rename, not a coincidence.

**All other systems:** DROP + CREATE for index renames, or user-written SQL.

For Djogi: index rename auto-detection is a reasonable future feature, following Django's
approach (compare all attributes except name; if everything else matches, emit `RENAME INDEX`
without prompting). The false-positive risk for indexes is low enough that a silent auto-detect
is defensible. This should be deferred until Djogi's index model is stable.

---

## False-positive guard rails

### Django: the interactive prompt is the guard rail

The only guard against a false rename is the developer at the terminal. `_boolean_input`
defaults to `False` if the user just presses Enter, making the conservative (destructive)
path the path of least resistance. This is correct default behavior — the developer must
actively confirm the rename.

In CI, `NonInteractiveMigrationQuestioner.ask_rename` returns `False` unconditionally
(`questioner.py:67-73`). This means every automated `makemigrations` run treats potential
renames as DROP + ADD. If a developer renames a field, commits without running `makemigrations`
interactively first, and the CI generates the migration, the result is a destructive migration
with no warning.

The documented mitigation: always run `makemigrations` interactively in development before
committing, and treat the generated migration file as source code to be reviewed in PR.

### Alembic: no guard needed because no detection exists

Because Alembic performs no rename detection, there is no false positive to guard against.
The user editing the migration to add a `RenameTableOp` is both the detection and the guard.
Any `include_name` or `include_object` hook in `env.py` that filters table names could
inadvertently suppress a DROP operation — but this is a filter configuration concern, not
a rename detection concern.

### Liquibase: the user writing `<renameColumn>` is the guard rail

Because the diff tool never emits `<renameColumn>`, there is no automatic rename to be wrong.
The user who writes `<renameColumn>` has explicitly confirmed the intent.

### Prisma: warnings as paper trail

Prisma's destructive-change classifier surfaces the rename scenario as a `warning` (data loss
possible but migration is executable). The warning is embedded in the generated migration file
as a SQL comment:

```sql
/*
  Warnings:

  - You are about to drop the column `viewCount20` on the `Blog` table. All the data in the
    column will be lost.
*/
```

(Source: `prisma.md`, `fixtures/existing-db-1-draft/prisma/migrations/20201203153838_draft/
migration.sql:1-7`. Confidence: high.)

This is a guard against accidental deployment: the warning is committed to git, visible in PR
review, and visible in the migration file history. It does not prevent the migration from running,
but it creates a paper trail. Djogi should adopt this pattern — embedding a warning comment
(e.g., `-- djogi: DROP COLUMN users.name: data loss if not a rename`) in generated migration
SQL when a column deletion is detected.

---

## Convergence / divergence

**Universal convergence:**

1. No surveyed tool has 100% reliable automatic rename detection in its autogenerate path.
2. Every tool that auto-generates migrations defaults to the destructive path (DROP + CREATE) for
   ambiguous renames.
3. Every explicit rename mechanism (Django's prompt, Liquibase's change type, Djogi's annotation)
   requires human intent to be expressed.

**Points of divergence:**

1. **Development UX vs CI safety:** Django optimizes for development UX (interactive prompt shows
   the candidate and asks). This is excellent ergonomics at the terminal and dangerous in CI.
   Djogi's annotation model optimizes for CI safety (no prompt needed, intent is in the source).

2. **Explicitness location:** Liquibase's intent is in the migration file (a separate artifact).
   Django's intent is expressed at generation time (interactive) and encoded in the generated
   migration file. Djogi's intent is in the model definition (the `#[field(renamed_from)]`
   attribute). Encoding rename intent in the model definition has the advantage that it is
   visible during code review without opening the migration file.

3. **Index rename vs column rename treatment:** Django treats index renames differently (silent
   auto-detect) from column renames (prompt). All others treat both the same. This divergence
   reflects the lower false-positive risk for index renames (index identity is fully structural).

4. **False-positive documentation:** No tool documents its false-positive rate for rename
   detection. Django SURPRISE 6 (`autodetector.py:113-125`) reveals one specific false-positive
   class (FK-target-stripped comparison matching models with different FK targets) that is
   undocumented in Django's own changelog.

---

## Djogi implications

### Validating the plan to NOT auto-detect renames

**Pros of Djogi's planned approach (no heuristic, explicit `#[field(renamed_from)]`):**

1. **Zero false positives.** No heuristic means no incorrect `ALTER ... RENAME` operations
   silently applied to production data. The absence of detection is the absence of risk.

2. **CI-safe by default.** No interactive prompt means `djogi makemigrations` can run in any
   environment without human intervention and produce reproducible output.

3. **Consistent with the majority.** Prisma, Alembic, Diesel, refinery, SeaORM, cot, and
   Liquibase's diff path all default to DROP + CREATE. Djogi is aligned with the conservative
   mainstream, not an outlier.

4. **Annotation is persistent intent.** `#[field(renamed_from = "old_name")]` lives in the struct
   definition. Future developers reading the code see that `full_name` was previously `name`.
   This is documentation as well as migration instruction.

5. **No false-positive class to explain or document.** Django's `only_relation_agnostic_fields()`
   false-positive class (SURPRISE 6) is a real bug that has silently affected users. Djogi has
   no equivalent to discover.

**Cons:**

1. **More manual work on rename.** The developer must both rename the field in the struct AND
   add `#[field(renamed_from = "old_name")]`. If they forget the annotation, the diff engine
   generates a destructive migration silently — the same outcome as all other non-detecting systems.

2. **No "did you mean rename?" hint.** Unlike Django's prompt, Djogi gives no hint that a
   column deletion might be an intended rename. A warning comment in the generated SQL (Prisma's
   paper trail pattern) partially mitigates this.

**Mitigation for the cons:**

- Embed a `-- djogi: WARNING: DROP COLUMN table.column — if this was a rename, annotate with
  #[field(renamed_from = "column")]` comment in the generated `_up.sql` file whenever a
  `DROP COLUMN` is emitted.
- Document clearly in the Djogi migration guide: "If you rename a field in your struct, add
  `#[field(renamed_from = "old_name")]` to generate `ALTER TABLE ... RENAME COLUMN` instead of
  `DROP COLUMN` + `ADD COLUMN`."

**Future opt-in:**

Consider a `djogi makemigrations --detect-renames` flag (medium priority, post-0.1.0) that:
1. Applies the "exactly one drop, exactly one add, same type, same nullability" heuristic.
2. Prints a list of candidates: "Possible rename: users.name → users.full_name (TEXT NOT NULL
   → TEXT NOT NULL). Use --accept-renames or add #[field(renamed_from)] to confirm."
3. Never silently emits `ALTER ... RENAME`. Always requires either the annotation or an explicit
   flag acknowledgment.

This is strictly opt-in and never runs in CI unless the CI pipeline explicitly enables it.

### Summary of Djogi's position

Djogi's planned default is the right default. It matches the conservative majority, eliminates
the entire false-positive class, and is safe for automated pipelines. The explicit annotation
model is superior to Django's interactive prompt (CI-safe) and superior to Liquibase's hand-edit
(intent is in the model, not a separate file). The one concrete improvement to the current plan is
to add a warning comment to generated DROP COLUMN migrations so the developer at least sees the
risk in the generated file.

---

## Open questions

**Q1: Is there a "safe" heuristic?**

The narrowest possible heuristic: "detect rename only if there is EXACTLY one dropped column and
EXACTLY one added column in the same table AND their SQL types are identical AND their nullability
is identical." This minimizes false positives but does not eliminate them. The `email_override`
→ `display_name` scenario above still fires it. The false-positive rate is unquantifiable without
production deployment data. No surveyed tool found this rate low enough to ship without a human
gate.

**Q2: Should Djogi warn on DROP COLUMN in generated SQL?**

Yes. The Prisma paper trail pattern (embedding a warning as a SQL comment in the generated
migration file) is low-cost and high-value. It creates a review artifact in git without changing
the migration's behavior. The warning should name the specific column and suggest the annotation
syntax. (This is a concrete enhancement to the current Djogi spec, resolvable in the migration
generator design.)

**Q3: How should `#[field(renamed_from)]` interact with multi-step renames?**

If a column is renamed from `a` to `b`, then later renamed from `b` to `c`, the annotation on the
current struct field would be `#[field(renamed_from = "a")]` if the first rename was never
dropped from the annotation, or `#[field(renamed_from = "b")]` if only the most recent rename
is preserved. The schema snapshot (`schema_snapshot.json`) should record the current column name,
so `renamed_from` needs to match only the previous name (what was in the snapshot). Multi-step
rename chains across migration history do not require chained annotations.

**Q4: Does `#[field(renamed_from)]` survive after the migration is applied?**

This is an open design question. Options:
- **Keep the annotation permanently:** self-documenting history, but the field definition is noisy.
- **Remove after the migration is committed:** clean struct, but the rename history is only in
  git log.
- **Move to a `#[field(history)]` sub-attribute:** explicit separation of migration hints from
  current semantics.

This is unspecified in the current Djogi spec and should be resolved during the migration
generator design phase.

**Q5: Is Django's silent `RenameIndex` auto-detect safe to adopt for Djogi?**

Yes, with low false-positive risk. Index identity is fully determined by the column list,
uniqueness, expression, and partial predicate. Two indexes on the same table that match on all
these dimensions but differ only in name are almost certainly the same index renamed. The Djogi
index model (once designed) should include silent rename detection for indexes using this same
comparison. This is strictly lower risk than column rename detection and does not require a
prompt or annotation.

---

## References

All claims in this document are drawn from primary source inspection of the following project notes:

- `projects/django.md` — Django commit `69d86004f7b3c9ed223c18998c2b799d1670474f`
- `projects/alembic.md` — Alembic commit `0ab90276fc583d52e31e95d3f59b4b6c00ec39ee`
- `projects/prisma.md` — Prisma (TypeScript) commit `62b44ac01aafbe101dad63abaab7da9747f62839`
  + Prisma Engines (Rust) commit `3c6e192`
- `projects/liquibase.md` — Liquibase commit `1d7330406e1bfc3648ba651a4b3b4fe495cbd1a8`
- `projects/diesel.md` — Diesel commit `df1f3ee56d8c8ae17dfab081de36a17668bfb31c`
- `projects/refinery.md` — refinery commit `c4f819bbbab3f67c98b4ff44a40cd83430f1172d`
- `projects/sea-orm.md` — SeaORM commit `3d33b516e969d936a97f2c89d968c269ed3f62c7`
- `projects/cot.md` — cot commit `5b3f957531908117e26085b78241c1d163ef1341`
- `projects/flyway.md` — Flyway commit `be2566341`
- `projects/sea-query.md` — sea-query commit `018efe989b842ea6b067eeae952dd82b81b4560b`
- `projects/sqlalchemy.md` — SQLAlchemy commit `deb949fe05ed8ff0f72f01d53f08f21ba8776aef`
