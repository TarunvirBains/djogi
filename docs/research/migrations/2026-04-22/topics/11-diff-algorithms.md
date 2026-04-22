# Topic 11: Diff Algorithms

## Executive summary

A migration diff algorithm answers one question: given what the database has now and what the developer wants, what SQL must run to close the gap? The question is simple; deriving both sides reliably is not.

The "to" side (desired state) is comparatively stable across all eleven surveyed systems: it comes from a live model definition, a declarative schema file, or a JSON descriptor. The "from" side (applied state) is where every system makes its most consequential architectural choice. Three dominant approaches exist: replay all migration files in-memory to reconstruct what the database must look like (Django, cot); introspect the live database directly (Alembic autogenerate, Diesel `--diff-schema`); or apply all migration files to a temporary shadow database and introspect that (Prisma). A fourth pattern — storing a snapshot of the schema at migration-generation time as a side-car artefact — appears in both cot (embedded in each migration file) and, for Djogi, as the planned `migrations/schema_snapshot.json`.

All eleven systems produce the same logical diff output: a list of operations (`CreateTable`, `AddColumn`, `DropColumn`, `AddForeignKey`, etc.). All systems that produce operations must then order them so FK-dependent tables are created before FK constraints, and FK constraints are dropped before the tables they reference. Most systems use topological sort to solve the ordering problem; Django uses `graphlib.TopologicalSorter` explicitly; cot uses a custom cycle-breaking + topological sort in `cot-cli/src/migration_generator.rs`.

Column matching is universally name-based across all eleven systems. No surveyed system matches columns by ordinal position. Rename detection is universally absent from auto-generation paths (Alembic, Prisma, Diesel, cot all treat a rename as drop + add); Django adds an interactive heuristic that is unsafe in CI.

Djogi's plan — diff a target descriptor JSON against a stored snapshot JSON, producing operations that become SQL migration files — is an instance of Approach D (stored snapshot) in a separate file rather than embedded in migration files. This approach is validated by Prisma's `migrations/schema_snapshot.json` analogue and by cot's snapshot-in-migration design. Its risk is snapshot/reality drift, which must be addressed by a `djogi verify` command.

**Confidence:** Citations throughout are high-confidence unless labelled otherwise.

---

## Comparison matrix

| System | Target source ("to") | Source-of-truth for "from" (applied state) | Diff algorithm type | Operation ordering | Handles renames? |
|---|---|---|---|---|---|
| **Alembic** | SQLAlchemy `MetaData` objects (live Python) | Live DB introspection via `Inspector` | Name-based column matching; `_autogen_for_tables()` + `_compare_columns()` | Operations emitted in dependency order; `sorted_tables` used | No — drop+add only (`compare/` has zero rename logic) |
| **Django** | `models.py` ORM classes (live Python) | In-memory `ProjectState` rebuilt by replaying all applied migrations | `MigrationAutodetector._detect_changes()` — fixed-order method sequence | `graphlib.TopologicalSorter` within app; chopping algorithm across apps | Interactive heuristic (field-signature comparison + `ask_rename`); defaults to drop+add in CI |
| **Prisma** | `schema.prisma` PSL file | Shadow DB: apply all migrations to temp DB, introspect result | AST-level diff via `SqlMigrationStep` enum; name-based | FK dependencies resolved before emitting steps | No — `DropColumn + AddColumn`; `SqlMigrationStep` has no `RenameColumn` variant |
| **Diesel** | `schema.rs` (Rust macros) | Live DB introspection via `infer_schema_internals` | `SchemaDiff` enum: `CreateTable`, `DropTable`, `ChangeTable` | Not documented; basic topological ordering implied | No — drop+add only |
| **cot** | `#[model]` Rust structs (live AST scan) | Snapshot structs embedded in prior migration files | Field-set diff on parsed `ModelInSource` structs | FK cycle-breaking + topological sort in `migration_generator.rs:1058-1115` | No — `RemoveField + AddField`; data loss, no warning |
| **Django (detail)** | — | — | `generate_renamed_fields()` → `generate_removed_fields()` → `generate_added_fields()` → `generate_altered_fields()` | `_sort_migrations()` + `_build_migration_list()` | Interactive heuristic only; `NonInteractiveMigrationQuestioner` defaults to `False` |
| **SeaORM** | Hand-written Rust code | Not applicable — no autogen | None (manual) | User-defined in `migrations()` `Vec` | Not applicable |
| **Flyway** | SQL files (no model layer) | Not applicable — no autogen | None (manual) | User-defined file prefix | Not applicable |
| **Liquibase** | Changelog XML/YAML; `generateChangelog` from snapshot | Live DB snapshot via `SnapshotGeneratorFactory` | Name-based; `DiffToChangeLog` | Operations emitted in dependency order | No — add+drop only in diff output; `RenameColumnChange` exists for hand-written changelogs only |
| **refinery** | SQL files | Not applicable — no autogen | None | User-defined file version prefix | Not applicable |
| **Djogi (planned)** | `target/djogi_models.json` (from `#[djogi::model]` descriptors) | `migrations/schema_snapshot.json` (stored snapshot) | JSON descriptor diff | Topological sort (planned) | Explicit `#[field(renamed_from = "old_name")]` annotation |

---

## The two-sided diff problem

Every diff algorithm needs two schema representations:

- **"From"** — what the database has now (the applied state)
- **"To"** — what the developer wants (the desired state)

The migration is the set of operations that transforms "from" into "to".

The "to" side is the easier side. It comes from wherever the developer declares their schema: ORM model code, a PSL file, a JSON descriptor. These change only when the developer writes code.

The "from" side is the hard side. The database may have been touched by many migrations, manual interventions, failed partial applies, or emergency hotfixes. Any technique that derives "from" must answer: whose truth are we starting from?

The choice of technique for the "from" side determines:
- Whether the system requires a live database connection at diff time
- Whether the system can detect drift between what was planned and what was actually applied
- How complex the system is to operate in CI/CD pipelines
- Whether the system is safe to use across multiple machines without shared state

---

## Approaches for deriving "from" (applied state)

### Approach A: Replay migrations in-memory (Django, cot)

**How it works:** Walk all on-disk migration files in topological order, apply each migration's `state_forwards(app_label, state)` operation to an in-memory schema representation. The final in-memory state is "from".

**Django:** `MigrationGraph.make_state(nodes, at_end)` (`django/db/migrations/graph.py:315-332`) generates the forward plan for all applied nodes and replays it by calling `project_state = self.nodes[node].mutate_state(project_state, preserve=False)` for each node. Each `Migration.mutate_state()` calls `operation.state_forwards(app_label, state)` for every operation. The `ProjectState` accumulates `ModelState` objects. There is no persistent snapshot — the state is reconstructed from scratch on every `makemigrations` or `migrate` run. This is O(n) in migration count and a known pain point at scale (noted as Django Surprise 5 in `django.md`).

**cot:** The CLI scans all `*.rs` files under `src/` using `syn::parse_file`, identifies structs annotated `#[model(model_type = "migration")]` inside `src/migrations/m_*.rs`, and treats those snapshot structs as the "from" representation. No in-memory replay of individual operations is needed; the snapshot struct is the final state at the time the migration was generated. (`cot-cli/src/migration_generator.rs:198-213`)

**Pros:**
- No database connection required at diff time
- Deterministic — same migration files always produce the same "from" state
- Works in CI without a live database

**Cons:**
- Replay can diverge from the actual database state if migrations were applied out of order, modified after application, or if manual DDL was run against the database
- O(n) cost in migration count for full replay (Django)
- In cot's approach, the snapshot struct must be kept in sync with the operations in the same migration file — hand-editing operations without updating the snapshot produces incorrect future diffs (cot Surprise 3 in `cot.md`)

**Confidence: high** — both approaches confirmed by reading source.

---

### Approach B: Live DB introspection (Alembic autogenerate, Diesel `--diff-schema`)

**How it works:** Connect to the live database, query `information_schema` (or equivalent), construct an in-memory metadata object from what the database actually contains. That metadata is "from".

**Alembic:** `compare_metadata()` (`autogenerate/api.py:50-173`) calls `_autogen_for_tables()` which calls `inspector.get_table_names()`, `inspector.get_columns()`, `inspector.get_foreign_keys()`, etc. (`compare/tables.py:36-83`). The Inspector queries the live database. The result is a set of `Table` objects representing what the database currently has. This is then compared against the SQLAlchemy `MetaData` object representing what the application wants.

**Diesel `--diff-schema`:** `diesel migration generate --diff-schema` uses `infer_schema_internals` to query the live database and constructs a representation of the current schema. This is then compared against the parsed `schema.rs` file. (`diesel_cli/src/migrations/diff_schema.rs:28-240`). Note: this is explicitly labelled "not production-ready" in the Diesel source (`diesel_cli/src/migrations/mod.rs:127-131`).

**Pros:**
- "From" is always accurate — it reflects what the database actually contains, not what a replay chain claims
- Detects drift from any source (manual DDL, failed migrations, tool changes)

**Cons:**
- Requires a database connection at `makemigrations` time, which may not be available in CI or on developer machines without a local DB
- Hides drift as a feature: the diff is generated against actual DB state, not against what migrations claim the DB should be — this can mask bugs where migrations have been selectively applied
- Does not validate that the migration history is internally consistent

**Confidence: high** — Alembic source read in full; Diesel confirmed.

---

### Approach C: Shadow DB (Prisma)

**How it works:** Create a temporary database, apply every migration in the migrations directory to it, then introspect the result. The introspected schema is "from". Compare "from" against the schema derived from `schema.prisma` to produce the next migration.

**Prisma mechanics** (verified from `prisma-engines-reference`):

1. `new_shadow_database_name()` generates a UUID-based name: `format!("prisma_migrate_shadow_db_{}", uuid::Uuid::new_v4())` (`flavour/postgres/connector/native/lib.rs:639-641`).
2. `CREATE DATABASE "{shadow_database_name}"` runs on the main connection.
3. Each migration script is applied to the shadow DB via `apply_migration_script` using Postgres's simple protocol, which splits the script into individual statements and runs each separately. (`flavour/postgres/connector/native/mod.rs:146-156`)
4. `describe_schema` introspects the shadow DB.
5. The shadow DB is always destroyed: `DROP DATABASE IF EXISTS "{name}" WITH (FORCE)` (Postgres ≥13) or `DROP DATABASE IF EXISTS "{name}"` fallback. (`flavour/postgres/connector/native/shadow_db.rs:95-112`)

The shadow DB is created and destroyed **per RPC call**. An in-process `MigrationSchemaCache` keyed on `DefaultHasher` of the migration directory list prevents re-creating the shadow DB on repeated calls within one engine session. (`commands/src/migration_schema_cache.rs`)

For **drift detection** (`diagnose_migration_history.rs:126-192`):
1. Apply only successfully-applied migrations (those with `finished_at IS NOT NULL AND rolled_back_at IS NULL`) to the shadow DB to get `from: SqlSchema`.
2. Introspect the real production DB to get the actual schema.
3. Diff the two schemas: `dialect.diff(from, to, &filter)`.
4. If non-empty, emit `DriftDiagnostic::DriftDetected { summary }`.

**Pros:**
- "From" is always consistent with the filesystem migration history — any discrepancy between what migrations claim and what the database has is drift, and it is surfaced explicitly
- Protects against ledger drift (where the `_prisma_migrations` table says a migration is applied but the schema doesn't match)
- Same approach works for both migration generation and drift detection

**Cons:**
- Requires `CREATE DATABASE` and `DROP DATABASE` permissions — not available in all cloud-managed Postgres environments (e.g., AWS RDS, Azure Flexible Server with restricted permissions)
- A user-configurable `shadowDatabaseUrl` is required for those environments, adding operational complexity (`packages/config/src/PrismaConfig.ts:21-29`)
- Creates and destroys a database on every invocation (unless the in-process cache is warm), which adds seconds to every `prisma migrate dev` run
- The shadow DB approach is a well-known Prisma user pain point in managed DB environments

**Confidence: high** — full shadow DB lifecycle read from `schema-engine/connectors/sql-schema-connector/src/flavour/postgres/connector/native/shadow_db.rs`.

---

### Approach D: Stored snapshot (cot embedded; Djogi side-car file)

**How it works:** At migration generation time, write a snapshot of the current schema state into a file. Future diff runs use this snapshot as "from" rather than replaying migrations or introspecting the database.

**cot** implements a per-migration embedded variant: the generated migration file contains both the operational plan (`const OPERATIONS: &[Operation]`) and a copy of each affected model struct annotated `#[model(model_type = "migration")]`. The CLI reads these snapshot structs to know "what was the model at the most recent migration." No separate snapshot file exists; the snapshot is distributed across all migration files. (`cot-cli/src/migration_generator.rs:198-213`, cot Surprise 3 in `cot.md`)

**Djogi's plan** uses a separate side-car file `migrations/schema_snapshot.json`. The snapshot is updated on every successful `djogi migrate` run. The diff compares:
- `target/djogi_models.json` (the current desired state, produced by `build.rs` from `#[djogi::model]` descriptors)
- `migrations/schema_snapshot.json` (the last-successfully-applied state)

Per the Phase 7 migration system design (`docs/superpowers/specs/2026-04-22-phase7-migration-system-design.md:49-55`): "Djogi does not plan by diffing directly against the live database catalog on every build or CLI invocation. The planner diffs: desired schema from descriptors [and] applied schema from `schema_snapshot.json`."

**Pros:**
- No database connection required at diff time
- No replay of all migration files (avoids O(n) cost)
- Deterministic and reproducible
- Snapshot is a first-class VCS artefact that shows reviewers what the schema looked like before and after
- Djogi's external snapshot file avoids cot's coupling problem (the snapshot is not inside migration files, so hand-editing migration operations does not corrupt future diffs)

**Cons:**
- The snapshot must be updated on every successful migration apply. If the update step is missed (e.g., because a migration was applied manually via the SQL shell and the snapshot was not regenerated), the snapshot diverges from actual database state
- The snapshot file must be checked into version control carefully — if two developers independently generate migrations on parallel feature branches, merge conflicts in the snapshot require manual resolution
- If the snapshot diverges from the database, the system cannot detect this without a `djogi verify`-style command that introspects the live database

**Confidence: high** — cot approach verified from source; Djogi approach from spec documents.

---

## Approaches for deriving "to" (desired state)

### From ORM models (Django, cot, SeaORM)

**Django:** `models.py` ORM classes are scanned by `MigrationAutodetector.changes()` which constructs a `ProjectState` from them. The state machine stores `ModelState` objects keyed by `(app_label, model_name)`. The autodetector diffs two `ProjectState` objects — one from applied history, one from live models. (`autodetector.py:62-72`)

**cot:** `#[model]`-annotated Rust structs are discovered by walking `src/**/*.rs` with `syn::parse_file`. The CLI identifies structs with `#[model]` or `#[model(model_type = "application")]` as the current desired state. (`cot-cli/src/migration_generator.rs:303-428`)

**SeaORM:** The ordered `Vec<Box<dyn MigrationTrait>>` is the authoritative list, but there is no autogen from entity structs — the "to" for migration generation is whatever the user writes manually.

### From declarative schema file (Prisma schema.prisma)

Prisma's `schema.prisma` (written in PSL — Prisma Schema Language) is the canonical desired-state declaration. The schema engine parses PSL into a Datamodel Management object, compiles it to a SQL schema AST, and uses that as "to" in the diff. All diff computation happens in the Rust engine (`prisma-engines-reference`). The TypeScript CLI only passes the file contents to the engine via JSON-RPC.

### From SQLAlchemy MetaData (Alembic)

The `AutogenContext.sorted_tables` (`autogenerate/api.py:486-499`) aggregates `MetaData.sorted_tables` across potentially multiple `MetaData` instances. The `MetaData` object is the Python in-memory representation of the desired schema. The user provides it in `env.py` via `context.configure(target_metadata=target_metadata)`.

### From Rust descriptors → JSON (Djogi)

Djogi's `#[derive(Model)]` proc macro emits a `Model::descriptor()` call via `inventory::submit!` and writes `target/djogi_models.json` as a side-channel output. `build.rs` reads this JSON to drive the diff. The JSON descriptor is the "to" side; `migrations/schema_snapshot.json` is the "from" side.

---

## Diff algorithms

### Alembic's autogenerate

**Entry point:** `autogenerate/api.py:50-173` — `compare_metadata()` → `produce_migrations()` → `AutogenContext` → `compare._populate_migration_script()`.

**Pipeline in order** (all citations confirmed by reading source):

1. `_populate_migration_script()` calls `_produce_net_changes()` which dispatches via a `PriorityDispatcher` (`autogenerate/compare/__init__.py:33-40`).
2. `_autogen_for_tables()` (`compare/tables.py:36-83`) calls `inspector.get_table_names()`, excludes the `alembic_version` table (`tables.py:52-55, 73`), and calls `_compare_tables()`.
3. `_compare_tables()` (`compare/tables.py:86-232`): tables in metadata but not connection → `CreateTableOp`; tables in connection but not metadata → `DropTableOp`; tables in both → call `_compare_columns()` and dispatch table-level comparators.
4. `_compare_columns()` (`compare/tables.py:235-307`): column in metadata but not connection → `AddColumnOp`; column in both → `AlterColumnOp` with type, nullable, server_default dispatch; column in connection but not metadata → `DropColumnOp`.
5. `_compare_indexes_and_uniques()` (`compare/constraints.py:53-441`): reflects `get_unique_constraints()` and `get_indexes()`, converts to `_constraint_sig` objects for comparison by name and column-set signature.
6. `_compare_foreign_keys()` (`compare/constraints.py:626-714`): reflects `get_foreign_keys()`, computes added/removed FKs by column+referent signature.

**Type comparison (`compare_type`):** As of Alembic 1.12.0, type comparison is on by default (`runtime/environment.py:580-582`). Type comparison is a two-stage dispatch: `_user_compare_type` (user-provided callable, `FIRST` priority) then `_dialect_impl_compare_type` (`LAST` priority). (`compare/types.py`)

**Server default comparison (`compare_server_default`):** Off by default (`runtime/environment.py:590-597`). Accepts `True`, `False`, or a callable `(context, inspected_col, metadata_col, inspected_type, metadata_type) -> bool|None`.

**Column matching:** Name-based. There is no ordinal or positional matching anywhere in `autogenerate/compare/`.

**Rename handling:** None. The `compare/` directory has zero rename detection logic. A renamed column appears as `DropColumnOp` + `AddColumnOp`. `RenameTableOp` exists in `ops.py:1451-1485` but is a hand-invoked operation only, never an autogenerate output.

**What is missed:** Check constraints (`render.py:440-442` raises `NotImplementedError()`); sequences; renames of any kind. Check constraints are reflected in `compare/util.py:30, 38` but no comparator dispatches on them.

**Confidence: high** — read all `compare/` submodules.

---

### Django's makemigrations

**Entry point:** `MigrationAutodetector.changes(graph, trim_to_apps, convert_apps, migration_name)` → `_detect_changes()` → `arrange_for_graph()`. (`autodetector.py:62-72`)

**`_detect_changes()` runs in a fixed method sequence** (`autodetector.py:182-231`):

```
generate_renamed_models()       # must run first — sets self.renamed_models
_prepare_field_lists()
generate_deleted_models()
generate_created_models()       # emits FK fields as separate AddField with _auto_deps
generate_deleted_proxies()
generate_created_proxies()
generate_altered_options()
create_renamed_fields()         # computes self.renamed_fields dict
create_altered_indexes()
create_altered_constraints()
generate_removed_constraints()
generate_removed_indexes()
generate_renamed_fields()       # emits RenameField ops if rename confirmed
generate_renamed_indexes()
generate_removed_fields()
generate_added_fields()
generate_altered_fields()
generate_added_indexes()
generate_added_constraints()
_sort_migrations()              # topological sort within each app via graphlib
_build_migration_list(graph)    # cross-app dependency resolution (chopping algorithm)
_optimize_migrations()          # MigrationOptimizer collapses CreateModel+AddField etc.
```

**FK dependency tracking:** `generate_created_models()` separates FK and M2M fields from the `CreateModel` body and emits them as `AddField` operations with `_auto_deps` pointing to the target model's create dependency. This guarantees the target table exists before the FK is added. (`autodetector.py:649-786`)

**Topological sort within an app:** `_sort_migrations()` uses `graphlib.TopologicalSorter` (Python 3.9+ stdlib). (`autodetector.py:417-433`)

**Cross-app dependency resolution:** `_build_migration_list()` uses a "chopping" algorithm — it iterates apps, collects operations whose deps are satisfied, chops them into a migration, repeats. If a full pass produces no progress, it enters `chop_mode` to force boundary placement. (`autodetector.py:297-415`)

**Optimization pass:** `MigrationOptimizer.optimize()` performs a forward scan: for each operation, scan right to find the first reducible pair using `Operation.reduce()`, collapse, restart. Example: `CreateModel` + `AddField` on the same model → single `CreateModel`. (`optimizer.py:12-69`)

**Rename detection — heuristic:** `create_renamed_fields()` compares field signatures ignoring FK targets (`only_relation_agnostic_fields()`) and calls `questioner.ask_rename()` interactively. `NonInteractiveMigrationQuestioner.ask_rename` defaults to `False` (no rename, treat as drop+add). (`autodetector.py:1048-1108`, `questioner.py:67-73`)

**Confidence: high** — read `autodetector.py`, `optimizer.py`, `graph.py`, `questioner.py`.

---

### Prisma's schema engine

**Entry point (TypeScript):** `MigrateDev` calls `createMigration` RPC → Rust engine receives `from: SchemaDatasource` (current PSL files) and `to: SchemaDatasource` (same) after applying shadow DB replay for `from`. (`packages/migrate/src/commands/MigrateDev.ts`)

**Diff computation (Rust):** The `SqlMigrationStep` enum (`schema-engine/connectors/sql-schema-connector/src/sql_migration.rs:481-516`) is the typed output:

```rust
// Confirmed variants — no RenameColumn
AlterTable(AlterTable)          // AddColumn, DropColumn, AlterColumn
RedefineTables(Vec<RedefineTable>)  // drop-and-recreate with INSERT...SELECT
CreateTable(CreateTable)
DropTable(DropTable)
RenameIndex(RenameIndex)
RenameForeignKey(RenameForeignKey)
CreateIndex(CreateIndex)
DropIndex(DropIndex)
AlterIndex(AlterIndex)
AddForeignKey(AddForeignKey)
DropForeignKey(DropForeignKey)
CreateNamespace(CreateNamespace)
DropNamespace(DropNamespace)
```

**Shadow DB path in full** (Postgres, no external shadow DB configured, `shadow_db.rs:53-113`):
1. Generate UUID-named shadow DB: `format!("prisma_migrate_shadow_db_{}", uuid::Uuid::new_v4())`
2. `CREATE DATABASE "{shadow_database_name}"`
3. Connect to shadow DB, apply each migration script using Postgres simple protocol (statement-by-statement, each auto-committed)
4. `describe_schema` introspects the shadow DB → produces `SqlSchema` ("from")
5. Parse `schema.prisma` → compile to `SqlSchema` ("to")
6. `dialect.diff(from, to, &filter)` → `SqlMigration` containing `SqlMigrationStep` list
7. `DROP DATABASE IF EXISTS "{name}" WITH (FORCE)` (Postgres ≥13)

**Advisory lock:** Prisma takes `SELECT pg_advisory_lock(72707369)` before `apply_migrations` and `mark_migration_applied`. (`flavour/postgres.rs:363-389`). The key `72707369` is a hardcoded magic number chosen by the Prisma team. Session-scoped, 10-second timeout.

**Rename handling:** Confirmed no `RenameColumn` in `SqlMigrationStep`. A PSL field rename using `@map` is diffed as `DropColumn` + `AddColumn`. (`sql_migration.rs:481-516`)

**Two-bucket destructive classifier:**
- `UnexecutableStepCheck` — operations that will fail with current data (`AddedRequiredFieldToTable` if row count > 0; `MadeOptionalFieldRequired` if null count > 0; etc.) — hard-blocked unless `--create-only`
- `SqlMigrationWarningCheck` — operations that may lose data but are executable (`NonEmptyColumnDrop`, `NonEmptyTableDrop`, `RiskyCast`, `UniqueConstraintAddition`, `EnumValueRemoval`) — prompt the user or require `--accept-data-loss`

Data probes (`COUNT(*)`, `COUNT(*) WHERE col IS NOT NULL`) run against the **production database**, not the shadow DB. (`sql_destructive_change_checker/unexecutable_step_check.rs:36-139`, `sql_destructive_change_checker/warning_check.rs:98-214`)

**Confidence: high** on shadow DB lifecycle and advisory lock (read from Rust source). Confidence **medium** on full diff internals not traversed.

---

### cot's migration generator

**Entry point:** `cot migration make` → `MigrationGenerator` in `cot-cli/src/migration_generator.rs`.

**Algorithm:**
1. Walk `src/**/*.rs` via `glob::glob("src/**/*.rs")` and `syn::parse_file`. Collect all structs annotated `#[model]` or `#[model(model_type = "application")]` as `app_models`. (`migration_generator.rs:303-428`)
2. Collect all structs annotated `#[model(model_type = "migration")]` from `src/migrations/m_*.rs` as `migration_models` (the "from" snapshot). (`migration_generator.rs:198-213`)
3. In `generate_operations()` (`migration_generator.rs:448-503`):
   - Model in app but not in snapshots → `CreateModel`
   - Model in both, fields differ → per-field diff: `AddField` or `RemoveField`
   - Model in snapshots but not in app → `RemoveModel`

**Dependency ordering** (`migration_generator.rs:1058-1115`): Circular FK dependencies are detected and broken by removing the FK from the `CreateModel` and emitting it as a later `AddField` operation — so both tables are created before the FK is added. Non-circular FK dependencies are resolved by topological sort.

**Field-type-change:** `make_alter_field_operation` (`migration_generator.rs:817-845`) hits `todo!()` at line 835 when a field exists in both app and snapshot but has changed type. This is an unimplemented panic shipped in cot 0.6.0. Any `cot migration make` invocation on a project with a type-changed field will crash the CLI.

**Rename handling:** None. `RemoveField` + `AddField` with data loss, no warning, no heuristic. (`migration_generator.rs:848-875`)

**Snapshot update:** On generation, the new migration file includes a copy of the current model struct as `#[model(model_type = "migration")]`. This snapshot is what future `migration make` invocations use as "from".

**Confidence: high** — read `migration_generator.rs` in full.

---

## Operation ordering

Operation ordering is not optional. Creating a table that has a FK before creating the table it references will fail at the database level. Dropping a column that is referenced by a FK before dropping the FK constraint will fail. Every system that autogenerates migrations must solve this.

**Django:** `_sort_migrations()` uses `graphlib.TopologicalSorter` within each app (`autodetector.py:417-433`). Cross-app ordering uses the chopping algorithm (`autodetector.py:297-415`) which iterates apps collecting operations whose dependencies are satisfied. `generate_created_models()` explicitly separates FK fields from `CreateModel` into later `AddField` operations with `_auto_deps` set. (`autodetector.py:649-786`)

**cot:** `GeneratedMigration::new` (`migration_generator.rs:1058-1115`) performs a custom cycle-breaking + topological sort. Steps:
1. Build a FK dependency graph among `CreateModel` operations.
2. Detect cycles (circular FK references between models).
3. Break cycles by removing the FK from one of the `CreateModel` operations and appending an `AddField` for that FK after all tables are created.
4. Topologically sort the remaining non-circular dependency graph.

**Alembic:** `AutogenContext.sorted_tables` (`api.py:486-499`) aggregates `MetaData.sorted_tables`, which respects FK dependencies. `MetaData.sorted_tables` returns tables in dependency order using a topological sort internally.

**Prisma:** The `SqlMigrationStep` list is ordered by the Rust engine. `CreateTable` steps for referenced tables precede `AddForeignKey` steps. `DropForeignKey` steps precede `DropTable` steps. The exact implementation is in the Rust diff engine (`schema-engine/connectors/sql-schema-connector/`) and not fully inspected, but the output ordering is correct per test fixtures.

**Liquibase:** `DiffToChangeLog` emits operations in dependency order. Tables without FK dependencies are created first; FK constraints are added after both tables exist. This is implicit in the `DiffToChangeLog.print()` logic.

**Django deferred SQL:** Django defers creation of indexes and unique constraints to after the main DDL block by accumulating them in `SchemaEditor.deferred_sql` (`schema.py:161, 169`), which is drained inside `SchemaEditor.__exit__` before the outer transaction commits. This prevents index creation from failing due to partial table state.

**Djogi implication:** Djogi's diff engine must produce operations in FK-safe order. The simplest approach: process `CreateTable` operations first in topological order, then `AddColumn` operations, then FK constraints, then indexes. `DropForeignKey` and `DropIndex` must precede the `DropTable` or `DropColumn` they guard.

---

## Shadow database: deep dive

Prisma's shadow database is the most rigorous approach to deriving "from" state. It is also the most operationally demanding. Understanding why Prisma chose it, and what it protects against, is essential for evaluating whether Djogi needs a similar mechanism.

**Why Prisma chose the shadow DB:**

The core problem the shadow DB solves is ledger drift: a database where the `_prisma_migrations` table says all migrations have been applied, but the actual schema differs from what those migrations would produce. This happens when:
- A DBA ran emergency DDL directly against the database
- A migration was partially applied (some statements succeeded, others failed)
- Two branches were merged and the combined migration history doesn't correspond to any sequential application
- An earlier version of the schema engine emitted different DDL than the current version would

A naive approach — compare the live database's schema directly against `schema.prisma` — does detect drift, but it cannot distinguish between "the user made intentional changes outside Prisma" and "the migration history is internally inconsistent." The shadow DB approach answers a different question: "does the current database match what you'd get by applying your migration files to a fresh database?" If the answer is no, the user knows their migration history is inconsistent with their current database, regardless of whether that inconsistency was intentional.

**What it protects against:** Every form of ledger drift. The shadow DB is the ground truth for "what my migration files would produce," and any deviation from that in the production database is surfaced as drift.

**Cost:**
- `CREATE DATABASE` permission is required. Cloud-managed Postgres (AWS RDS, Azure Flexible Server, GCP Cloud SQL) in restricted configurations may not grant this permission to application users.
- A user-configurable `shadowDatabaseUrl` is the workaround for restricted environments, but it requires the user to provision and maintain a second database.
- Shadow DB creation and destruction adds seconds to every `prisma migrate dev` invocation. The in-process `MigrationSchemaCache` (keyed on migration directory hash) reduces repeated calls within a session, but cold invocations pay the full cost.
- This is a well-documented Prisma user pain point in enterprise and cloud-managed environments.

**Do other surveyed systems implement shadow DB?** No. Liquibase's `generateChangelog` introspects the live database directly. Alembic autogenerate introspects the live database. Django reconstructs state in-memory from migration file replay. None of the other ten systems create a temporary database for diff purposes.

**SQLAlchemy's ephemeral test DB:** SQLAlchemy supports creating in-memory SQLite databases for testing, and test utilities can apply migrations to them and inspect the results. This is conceptually similar to the shadow DB pattern but is a testing utility, not a production diff mechanism. It does not replace the shadow DB for drift detection purposes.

**Djogi's position:** For v0.1.0, Djogi should not implement a shadow DB. The snapshot-based approach is adequate, and the shadow DB's operational cost is not justified at the early stage. A `djogi verify` command (discussed below) that introspects the live database and diffs against the snapshot is the right medium-term addition — it provides drift detection without requiring shadow DB creation permissions.

---

## Handling complex types

### ENUMs (Postgres)

**Alembic:** `autocommit_block()` must be used for `ALTER TYPE ... ADD VALUE` on Postgres < 12 because this statement cannot run inside a transaction. Alembic's `PostgresqlParser` (analogous) detects this. In `env.py`, users must use `op.get_context().autocommit_block()` for `ALTER TYPE ... ADD VALUE` when targeting older Postgres. (`runtime/migration.py:279-370`)

**Prisma:** `EnumValueRemoval` is a warning-class destructive change (always warns, no data probe) in the two-bucket classifier. (`warning_check.rs:7-48`). The Rust engine emits DDL to remove enum values, but warns the user because removing an enum value may fail at runtime if any rows contain that value.

**sea-query:** `TypeCreateStatement`, `TypeDropStatement`, `TypeAlterStatement` are first-class builders. `ALTER TYPE ... ADD VALUE IF NOT EXISTS` is supported. (`src/backend/postgres/types.rs`) cot and Djogi do not currently support ENUM types in their migration generators.

**Djogi implication:** ENUMs are not handled in the descriptor diff for v0.1.0. When Djogi adds ENUM support, the diff must account for `ALTER TYPE ... ADD VALUE` being non-transactional on Postgres < 14.

### Arrays

**Alembic:** `ARRAY` column types are representable in SQLAlchemy `MetaData` and will be diffed if `compare_type=True`. The diff may produce an `AlterColumnOp` for array type changes.

**sea-query:** `ColumnType::Array(elem_type)` with `postgres-array` feature. (`src/backend/postgres/table.rs:97-100`)

**cot:** No array support in `ColumnType` enum. The type list contains only cross-database scalar types.

**Djogi:** Arrays are planned as first-class types. The descriptor diff must handle `ARRAY` column type changes.

### JSONB

**cot:** No JSONB support. `ColumnType` has no JSONB variant. (`cot/src/db.rs:2082-2119`)

**sea-query:** `ColumnType::JsonBinary` → `"jsonb"`. (`src/backend/postgres/table.rs:95`)

**Alembic:** JSONB is representable as `postgresql.JSONB()` in SQLAlchemy `MetaData` and will be diffed if `compare_type=True`.

**Djogi:** `Jsonb<T>` is a first-class type. The descriptor diff must handle JSONB columns.

### Composite types

No surveyed system autogenerates migrations for Postgres composite types (`CREATE TYPE ... AS (col1 type1, col2 type2)`). Liquibase has a `CreateTypeDependentTableChange` for some cases, but composite type creation is generally hand-written in all systems.

---

## Convergence and divergence

**Universal across all systems that autogenerate:**
- **Name-based column matching** — no surveyed system uses ordinal/positional matching
- **Operation-based output** — all systems produce a list of discrete operations, not a "patch" of raw SQL with embedded newlines
- **Dependency-ordered operations** — all systems that produce FK-bearing operations sort them topologically

**Split across systems:**

| Topic | Systems that do it | Systems that don't |
|---|---|---|
| Shadow DB for "from" | Prisma only | All others |
| Live DB introspection for "from" | Alembic, Diesel | All others |
| In-memory replay for "from" | Django, (partially cot) | All others |
| Stored snapshot for "from" | cot (embedded), Djogi (planned) | All others |
| Interactive rename detection | Django | All others |
| Two-bucket destructive classifier | Prisma | All others |
| Type comparison on by default | Alembic (since 1.12.0) | Most |
| Check constraint autogeneration | None (Alembic raises `NotImplementedError`) | All |
| Sequence autogeneration | None | All |

---

## Djogi implications

### Validating the side-car file approach (Approach D)

Djogi's `migrations/schema_snapshot.json` is structurally the same decision as cot's embedded snapshot structs, with three important differences that make Djogi's version stronger:

1. **Decoupled from migration files:** cot's snapshot is inside each migration file. If a developer hand-edits the migration's operations without updating the snapshot, future `cot migration make` invocations will generate incorrect diffs. Djogi's external snapshot file is only updated on successful apply — it cannot be accidentally desynchronised by editing a migration file.

2. **No O(n) replay cost:** Django's in-memory replay is O(n) in migration count. cot's embedded snapshot avoids replay but requires the last migration file to contain the full model state. Djogi's snapshot file is read directly — O(1) regardless of migration count.

3. **First-class VCS artefact:** The snapshot file appears in git history, making it visible in code review. Reviewers can see both the SQL migration (`NNNN_name_up.sql`) and the resulting schema state (`schema_snapshot.json`) in the same PR. cot's distributed-across-migration-files approach achieves a similar effect but requires reading multiple files.

**Comparison with Prisma:** Prisma's shadow DB achieves higher correctness (it detects drift from any source), but requires elevated DB permissions that are unavailable in many production environments. Djogi's snapshot approach is simpler, requires no live DB connection at diff time, and is adequate for the v0.1.0 scope. The correctness gap is bridgeable by a `djogi verify` command.

**Comparison with Alembic:** Alembic's live DB introspection is accurate but requires a DB connection at `alembic revision --autogenerate` time. In CI pipelines that lack a live database during the diff phase, this is a blocker. Djogi's snapshot approach is always available offline.

**Comparison with Django:** Django's in-memory replay is correct as long as migration files are not modified after application. The O(n) cost grows with project age. At 500+ migrations (a realistic number for a 5-year Django project), replay takes noticeable time. Djogi's snapshot is O(1).

### Required handling for the snapshot invariant

The snapshot must be updated by every `djogi migrate` apply, not just by `djogi migration generate`. If a developer runs migrations manually via the SQL shell, updates the `djogi_migrations` ledger table directly, and does not regenerate the snapshot, the snapshot will diverge from the database. This is the primary operational risk.

Mitigation strategies:

1. **`djogi verify` command:** Introspect the live database and diff against `migrations/schema_snapshot.json`. Surface any discrepancy as a drift report. This does not require elevated permissions (no `CREATE DATABASE` needed — just `SELECT` on `information_schema`). Analogous to Alembic's `alembic check` (`command.py:323-378`) which raises `AutogenerateDiffsDetected` if the autogenerate diff is non-empty.

2. **CI gate:** `djogi migrate check` (or `djogi verify`) should be a required CI step that fails the build if the snapshot and the database are inconsistent. This is the same pattern as Alembic's `alembic check` used as a CI gate.

3. **Snapshot update on every successful apply:** The migration runner must atomically update `schema_snapshot.json` as part of the `djogi migrate` command, in the same step as updating the `djogi_migrations` ledger table. The snapshot must never be "behind" the ledger.

### The `djogi verify` command

This is the lightweight drift detection path that avoids the shadow DB complexity:

1. Connect to the live database.
2. Introspect the current schema from `information_schema` (tables, columns, types, constraints, indexes).
3. Load `migrations/schema_snapshot.json`.
4. Diff the introspected schema against the snapshot.
5. If any discrepancy: report what has drifted (columns added outside Djogi, tables dropped, types changed).

This surfaces drift without touching the generation path. It is read-only. It does not require `CREATE DATABASE` permissions. It is the correct CI safety net for teams that allow DBAs to make manual changes.

**Contrast with Prisma's drift detection:** Prisma computes `expected schema = shadow DB replay of migration files`, then diffs against live DB. Djogi's `verify` computes `expected schema = schema_snapshot.json`, then diffs against live DB. The difference: Djogi's approach trusts the snapshot rather than the migration files. If the snapshot accurately reflects the last successful apply (which it should, if the runner updates it correctly), the two approaches are equivalent. If the snapshot diverges from the migration files (which should not happen but is possible if the snapshot was edited manually), Djogi's approach gives a false clean result. A full `djogi verify --from-migrations` mode could optionally replay migration files to compute the expected schema, matching Prisma's approach.

---

## Open questions

1. **Snapshot merge conflicts on parallel branches.** If two developers independently generate migrations on parallel feature branches, both modify `schema_snapshot.json`. On merge, there will be a conflict. The resolution strategy needs documentation: the post-merge snapshot must reflect the merged schema, which means the conflict resolution is not a simple "pick one side" — it requires understanding both schemas and constructing the correct merged snapshot. This is a known pain point in Django projects that use parallel feature branch migrations.

2. **Manual SQL shell apply + snapshot not regenerated.** If a developer applies the up-migration SQL directly via `psql`, the `djogi_migrations` ledger may be updated (if they insert the row manually) but `schema_snapshot.json` will not be. The runner must detect this mismatch on the next `djogi migrate` invocation and either refuse to proceed or require explicit `djogi migrate --regen-snapshot`. `djogi verify` in CI should catch this, making it a required gate.

3. **Snapshot format stability.** If the `target/djogi_models.json` descriptor format changes (e.g., a new field type is added, or JSON key names change), existing `schema_snapshot.json` files become unreadable. The snapshot format must be versioned from day one. Liquibase's `V:hex` checksum versioning (`ChecksumVersion.java:12-22`, format `9:hash`) is the right pattern: prefix the snapshot with a format version so the diff engine knows how to deserialize it.

4. **Type comparison depth.** Alembic's `compare_type=True` compares column types using `_user_compare_type` and `_dialect_impl_compare_type` (`compare/types.py`). What Alembic considers "same type" is dialect-dependent and sometimes surprising (e.g., `VARCHAR(255)` vs `TEXT` may compare equal on some backends). Djogi's type comparison in the JSON diff must define precisely what constitutes a type change: `TEXT` vs `VARCHAR(255)` are different types in Postgres and should trigger a diff.

5. **Index descriptor in JSON.** If the descriptor JSON does not encode indexes (only table + column structure), the diff engine will not detect missing or extra indexes. For v0.1.0, this may be acceptable if indexes are always added via explicit migration annotations. But once users start adding indexes to their model descriptors, the diff engine must compare them.

6. **Destructive operation classification.** Prisma's two-bucket classifier (`warnings` vs `unexecutableSteps`) probes the production database with `COUNT(*)` queries at `evaluateDataLoss` time. Djogi should implement a similar classifier. For v0.1.0, a conservative approach — warn on all `DropColumn`, `DropTable`, and `AlterColumn` (type change) operations — is acceptable. The Prisma data-probe approach is strictly better for production deployments but requires a DB connection at diff time.

---

## Confidence summary

| Section | Confidence | Basis |
|---|---|---|
| Alembic autogenerate pipeline | high | Read all `autogenerate/compare/` submodules and `api.py` |
| Alembic `compare_type` default change | high | Read `runtime/environment.py:580-582` |
| Django `_detect_changes()` order | high | Read `autodetector.py:127-451` |
| Django topological sort via `graphlib` | high | Read `autodetector.py:417-433` |
| Django rename heuristic | high | Read `questioner.py`, `autodetector.py:1048-1108` |
| cot snapshot-in-migration approach | high | Read `migration_generator.rs:198-213` |
| cot `todo!()` on type change | high | Read `migration_generator.rs:835` |
| cot FK cycle-breaking + toposort | high | Read `migration_generator.rs:1058-1115` |
| Prisma shadow DB lifecycle | high | Read `shadow_db.rs:53-113` (prisma-engines) |
| Prisma advisory lock key | high | Read `flavour/postgres.rs:363-389` |
| Prisma SHA-256 checksum | high | Read `schema-connector/src/checksum.rs` |
| Prisma `SqlMigrationStep` enum | high | Read `sql_migration.rs:481-516` |
| Prisma two-bucket classifier | high | Read `unexecutable_step_check.rs:7-139`, `warning_check.rs:7-214` |
| Prisma statement-by-statement apply | high | Read `flavour/postgres/connector/native/mod.rs:146-156` |
| Prisma no `RenameColumn` | high | Read `sql_migration.rs:481-516` — variant absent |
| Alembic check constraint `NotImplementedError` | high | Read `render.py:440-442` |
| Diesel `--diff-schema` "not production-ready" label | high | Read `diesel_cli/src/migrations/mod.rs:127-131` |
| Liquibase checksum covers parsed DSL not SQL | high | Read `ChangeSet.generateCheckSum()` |
| Prisma out-of-order silent in `migrate deploy` | high | Read `apply_migrations.rs:36-45`, `diagnose_migration_history.rs` |
| Djogi snapshot approach (planned) | medium | Spec documents only; implementation not yet written |
