# cot

## Metadata

- **Clone path:** `/home/tarunvir/projects/cot-reference/`
- **Commit SHA:** `5b3f957531908117e26085b78241c1d163ef1341` (`feat: derive Ord and PartialOrd for Auto (#544)`)
- **Primary language:** Rust
- **Version:** `0.6.0` (`cot/Cargo.toml:3`)
- **Workspace crates relevant to migrations:**
  - `cot` — runtime runner, ledger model, `Operation` types, `MigrationEngine` (2,453 LOC in `migrations.rs` + 372 LOC in `sorter.rs`)
  - `cot-cli` — static-analysis diff engine, code generation, CLI commands (2,213 LOC in `migration_generator.rs`)
  - `cot-macros` — `#[migration_op]` attribute macro (40 LOC)
  - `cot-codegen` — model/field parsing shared between CLI and macros (`model.rs`, `expr.rs`, `symbol_resolver.rs`)
- **Stability:** Pre-1.0, actively maintained. No explicit stability disclaimer in README. Version `0.6.0` implies pre-stable API. The `make_alter_field_operation` function contains a hard `todo!()` panic (`cot-cli/src/migration_generator.rs:835`), indicating field-type-change migrations are not yet implemented.
- **Driver stack:** `sqlx` (via `sea-query-binder`) + `sea-query` for DDL. Feature flags: `sqlite`, `postgres`, `mysql` (all enabled by default). `cot/Cargo.toml:100-108`.

---

## Architecture

**Workspace layout** (`Cargo.toml:1-19`):

```
cot/src/db/migrations.rs          # MigrationEngine, Operation, ledger model
cot/src/db/migrations/sorter.rs   # Topological sort of migration graph
cot-cli/src/migration_generator.rs # Static-AST diff + Rust code generation
cot-macros/src/migration_op.rs    # #[migration_op] attribute macro
cot-codegen/src/model.rs          # Model/field descriptor parsing
cot-codegen/src/symbol_resolver.rs # Use-statement resolution for AST parsing
```

**Key roles:**

| File | Role |
|---|---|
| `cot/src/db/migrations.rs` | `MigrationEngine::run()`, `Operation` builders, ledger (`AppliedMigration`), DDL dispatch via sea-query |
| `cot/src/db/migrations/sorter.rs` | Topological sort of migration DAG by dependency declarations |
| `cot-cli/src/migration_generator.rs` | CLI diff: parse all `src/**/*.rs`, extract `#[model]` structs, compare against migration snapshot structs embedded in existing migration files, emit new Rust migration file |
| `cot-macros/src/migration_op.rs` | Rewrites `async fn foo(ctx: MigrationContext<'_>) -> Result<()>` into a boxed-pin function suitable as `CustomOperationFn` |
| `cot-cli/src/args.rs` | CLI subcommands: `cot migration list`, `cot migration make`, `cot migration new` |

**Derive/macro pipeline at high level:**

1. User annotates a struct with `#[model]` attribute macro (`cot-macros/src/lib.rs` → `cot-macros/src/model.rs`)
2. Macro expands struct into `impl Model for Foo { const TABLE_NAME; const COLUMNS; fn from_db(...); ... }`
3. CLI `cot migration make` calls `MigrationGenerator`, which walks `src/**/*.rs` using `syn::parse_file`, finds all `#[model]` structs, and also finds structs annotated with `#[model(model_type = "migration")]` (the snapshot structs embedded in prior migration files)
4. Diff is computed in memory; a new `.rs` file is emitted containing both the `impl Migration { OPERATIONS: &[...] }` block and a snapshot copy of each modified struct

---

## State model (source-of-truth)

**Confidence: high**

cot has two sources of truth that must be reconciled:

1. **Application models** — the live `#[model]`-annotated structs in `src/**/*.rs`. These are the "desired" state.
2. **Migration snapshots** — copies of model structs embedded in each migration file under `src/migrations/`, annotated with `#[model(model_type = "migration")]`. cot reads these to reconstruct "previously migrated state" without touching a database or schema file. (`cot-cli/src/migration_generator.rs:198-213`)
3. **DB ledger** — the `cot__migrations` table records which migrations have been applied.

The diff pipeline (`generate_migrations_as_generated_from_files`) compares (1) against the latest version of each model across (2). There is **no separate JSON descriptor file** and **no build.rs step** — the snapshot structs live directly inside migration Rust files.

**How cot decides "applied vs pending":**

At runtime `MigrationEngine::run()` calls `is_migration_applied()` which runs a `SELECT` on `cot__migrations` by `(app, name)`. If not present, the migration is applied. (`cot/src/db/migrations.rs:218-228`)

**Relationship to sea-query/sea-schema:**

- sea-query is used **only** for DDL emission at runtime (generating SQL strings for `CREATE TABLE`, `ALTER TABLE ADD COLUMN`, `ALTER TABLE DROP COLUMN`, `DROP TABLE`). (`cot/src/db/migrations.rs:490-530`)
- sea-schema is **not used at all** — no introspection of the live database schema. Grepped: `grep -rn "sea_schema" /home/tarunvir/projects/cot-reference/` returned no results.

---

## Ledger / history table

**Confidence: high**

The ledger table is defined as a `struct` with `#[model(table_name = "cot__migrations", model_type = "internal")]` and instantiated via a constant `Operation` at startup:

```rust
// cot/src/db/migrations.rs:1997-2021
#[derive(Debug)]
#[model(table_name = "cot__migrations", model_type = "internal")]
struct AppliedMigration {
    #[model(primary_key)]
    id: Auto<i32>,
    app: String,
    name: String,
    applied: chrono::DateTime<chrono::FixedOffset>,
}

const CREATE_APPLIED_MIGRATIONS_MIGRATION: Operation = Operation::create_model()
    .table_name(Identifier::new("cot__migrations"))
    .fields(&[
        Field::new(Identifier::new("id"), <Auto<i32> as DatabaseField>::TYPE)
            .primary_key()
            .auto(),
        Field::new(Identifier::new("app"), <String as DatabaseField>::TYPE),
        Field::new(Identifier::new("name"), <String as DatabaseField>::TYPE),
        Field::new(
            Identifier::new("applied"),
            <chrono::DateTime<chrono::FixedOffset> as DatabaseField>::TYPE,
        ),
    ])
    .if_not_exists()
    .build();
```

**Columns and their purposes:**

| Column | Type | Purpose |
|---|---|---|
| `id` | `Auto<i32>` (SERIAL / INTEGER AUTO_INCREMENT) | Surrogate PK |
| `app` | `String` (TEXT) | Name of the crate/app the migration belongs to |
| `name` | `String` (TEXT) | Migration name, e.g. `m_0001_initial` |
| `applied` | `chrono::DateTime<chrono::FixedOffset>` (TIMESTAMPTZ) | Wall-clock time migration was applied |

**Notably absent:** No checksum column. No execution duration column. No success/failure flag. No out-of-order marker. No partial-apply marker. The ledger is purely an applied/not-applied record keyed by `(app, name)`.

**Table name:** `cot__migrations` (double underscore, cot's namespace convention for internal tables).

---

## Execution

**Confidence: high**

### Lock strategy

No advisory lock, no `LOCK TABLE`, no filesystem lock. Grepped:

```
grep -rn "pg_advisory\|LOCK TABLE\|pg_try_advisory\|advisory_lock" /home/tarunvir/projects/cot-reference/
# returned zero results
```

Concurrency is entirely unhandled. Two simultaneous `MigrationEngine::run()` calls against the same database can race.

### Transaction boundaries

No explicit transaction wrapping of migrations. Each DDL operation is issued directly via `database.execute_schema(query).await?` followed by a separate INSERT into `cot__migrations`. (`cot/src/db/migrations.rs:208-212`)

If a DDL operation succeeds but the ledger INSERT fails (or vice versa), the migration is left in an inconsistent state. There is no per-migration DDL transaction, no savepoint, and no batch transaction.

### Non-transactional DDL handling

Not addressed — there is no mechanism to detect or opt out of transaction-wrapped DDL for databases where DDL is non-transactional (e.g. MySQL).

### Concurrency posture

None. Not documented, not handled.

### Async model

- **Executor:** tokio (`tokio = { workspace = true, features = ["rt-multi-thread"] }`, `Cargo.toml:61`)
- **Driver:** `sqlx` with `runtime-tokio` feature (`cot/Cargo.toml:56`)
- All async migration operations run on tokio's multi-thread runtime via axum's integration

---

## Recovery

**Confidence: high (absence proven by grep)**

### Checksum

No checksum computation anywhere in the codebase. Grepped:

```
grep -rn "checksum\|hash\|sha\|md5\|crc" /home/tarunvir/projects/cot-reference/ --include="*.rs"
# returns only auth-related blake3 password hashing — zero migration-related hits
```

If a migration file is manually edited after being applied, cot has no way to detect the discrepancy.

### Repair / stamp / baseline / fake

None of these workflows exist. The CLI offers only three migration subcommands (`cot-cli/src/args.rs:46-53`):

- `cot migration list` — lists migration files on disk
- `cot migration make` — generates a new migration by diffing model state
- `cot migration new <name>` — creates an empty custom migration stub

There is no `cot migration fake`, `cot migration repair`, `cot migration baseline`, or `cot migration stamp`. No mechanism to mark a migration as applied without executing it.

### Partial-apply handling

No partial-apply tracking. The ledger INSERT happens after all operations in a migration complete (`cot/src/db/migrations.rs:208-212`). If an operation fails mid-migration, the migration is not marked applied, but any DDL already executed is not rolled back.

### Out-of-order policy

Not configurable. Migrations are sorted topologically by their declared dependencies before execution (`cot/src/db/migrations/sorter.rs:55-61`). No `allow_out_of_order` flag.

---

## Diff and generation

**Confidence: high**

### Autogen from Rust descriptors

Yes. The CLI reads all `*.rs` files under `src/` using `glob::glob("src/**/*.rs")` and `syn::parse_file`. It identifies:

- **Application models:** structs annotated with `#[model]` or `#[model(model_type = "application")]`
- **Migration snapshots:** structs annotated with `#[model(model_type = "migration")]` embedded in `src/migrations/m_*.rs`

(`cot-cli/src/migration_generator.rs:303-428`)

The diff is computed in `generate_operations`:
- Model in app but not in snapshots → `CreateModel`
- Model in both, but fields differ → `AddField` or `RemoveField` per changed column
- Model in snapshots but not in app → `RemoveModel`

(`cot-cli/src/migration_generator.rs:448-503`)

**No build.rs step.** The diff runs entirely as a CLI tool.

### Dependency resolution and ordering

Within a migration, operations are topologically sorted by foreign key relationships. If model A has a FK to model B, the `CreateModel(B)` operation is guaranteed to precede `CreateModel(A)`. Circular FK dependencies are broken by removing the FK from the `CreateModel` and emitting it as a separate `AddField` operation after both tables exist. (`cot-cli/src/migration_generator.rs:1058-1115`)

Between migrations, topological sort is done by `MigrationDependency` declarations. Each auto-generated migration depends on the most recently generated migration for the same app. (`cot-cli/src/migration_generator.rs:998-1010`)

### Rename handling

**None.** No heuristic rename detection, no explicit annotation. A field rename produces a `RemoveField` followed by an `AddField` — i.e., drop-and-recreate with data loss.

### Destructive-operation detection

**None.** `RemoveField` and `RemoveModel` are generated silently. There is no warning system, no "unexecutable steps" bucket, and no Prisma-style destructive classifier. The `make_remove_field_operation` function simply constructs the operation without any diagnostic output beyond a `print_status_msg(StatusType::Removing, ...)`. (`cot-cli/src/migration_generator.rs:848-875`)

### Field-type-change migrations

`make_alter_field_operation` — the code path for when a field exists in both the app and snapshot but has changed type — hits `todo!()` at line 835:

```rust
// cot-cli/src/migration_generator.rs:817-845
fn make_alter_field_operation(
    _app_model: &ModelInSource,
    app_field: &Field,
    migration_model: &ModelInSource,
    migration_field: &Field,
) -> Option<DynOperation> {
    if app_field == migration_field {
        return None;
    }
    // ...
    todo!();
    // ...
}
```

This means `cot migration make` will **panic** if the type of an existing field changes. Only add/remove field operations are implemented.

### CLI command for generation

```
cot migration make [--path PATH] [--app-name NAME] [--output-dir DIR]
cot migration new <name> [--path PATH] [--app-name NAME]
```

Output goes to `src/migrations/m_NNNN_<name>.rs` and `src/migrations.rs` is regenerated to include all discovered migration modules.

---

## Schema metadata

**Confidence: high**

### Composite unique

Not supported as a migration operation. `Field::unique()` marks a single column as `UNIQUE`. There is no `UniqueConstraint` builder in `Operation` or `OperationInner`. (`cot/src/db/migrations.rs:659-915`)

### Composite indexes

No index-creation operation at all. The only DDL operations are `CreateModel`, `AddField`, `RemoveField`, `RemoveModel`, and `Custom`.

### Reflection / introspection

None. sea-schema is not a dependency. cot never reads the live database schema to compute the desired-vs-actual diff — it relies entirely on the snapshot structs embedded in migration files.

### Postgres-specific types

The `ColumnType` enum (`cot/src/db.rs:2082-2119`) contains only cross-database types:

```
Boolean, TinyInteger, SmallInteger, Integer, BigInteger,
TinyUnsignedInteger, SmallUnsignedInteger, UnsignedInteger, BigUnsignedInteger,
Float, Double, Time, Date, DateTime, DateTimeWithTimeZone, Text, Blob, String(u32)
```

No JSONB, no arrays, no pgvector, no UUID, no INET, no HSTORE. Grepped: `grep -rn "JSONB\|jsonb\|pgvector" /home/tarunvir/projects/cot-reference/` — zero results.

Unsigned integers are silently coerced to signed when targeting Postgres (`cot/src/db/impl_postgres.rs:31-42`).

### Composite primary keys

Explicitly rejected at compile time by the derive macro: `"composite primary keys are not supported; only one primary key field is allowed"` (`cot-codegen/src/model.rs:134`).

---

## Online-safe / staged migration guidance

**Confidence: high**

No `CONCURRENTLY` support anywhere. No documented online-safe patterns. Grepped `CONCURRENTLY` — returned only unrelated concurrency mentions in non-migration code.

The `Custom` operation type (`Operation::custom(forwards).backwards(backwards).build()`) is the only escape hatch for writing hand-crafted SQL that could include `CREATE INDEX CONCURRENTLY`, but the framework provides no guidance or tooling for this.

---

## Rust-specific concerns

**Confidence: high**

### Async model

Tokio, multi-thread runtime. `CustomOperationFn` is defined as:

```rust
// cot/src/db/migrations.rs:654-657
pub type CustomOperationFn =
    for<'a> fn(
        MigrationContext<'a>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
```

The `#[migration_op]` macro rewrites `async fn` into this boxed-pin form.

### Type-safety surface

The `Migration` trait uses `const` fields, meaning migration definitions are fully evaluated at compile time:

```rust
// cot/src/db/migrations.rs:1697-1709
pub trait Migration {
    const APP_NAME: &'static str;
    const MIGRATION_NAME: &'static str;
    const DEPENDENCIES: &'static [MigrationDependency];
    const OPERATIONS: &'static [Operation];
}
```

`Operation` and `Field` are `Copy` types with `const` builders, so the entire migration definition is a compile-time constant.

### Macro use

- `#[model]` is an attribute macro (not a derive) that rewrites the struct and emits `impl Model` boilerplate. (`cot-macros/src/lib.rs`)
- `#[model(model_type = "migration")]` marks snapshot structs embedded in migration files
- `#[migration_op]` is an attribute macro that rewrites `async fn` into boxed-pin form
- `query!` macro generates type-safe query expressions at compile time
- No `OUT_DIR` codegen / `build.rs` in the migration pipeline

### Feature flags and driver selection

```toml
# cot/Cargo.toml:100-108
[features]
default = ["sqlite", "postgres", "mysql", "json"]
db = ["dep:sea-query", "dep:sea-query-binder", "dep:sqlx"]
sqlite = ["db", "sea-query/backend-sqlite", ...]
postgres = ["db", "sea-query/backend-postgres", ...]
mysql  = ["db", "sea-query/backend-mysql", ...]
```

All three backends are enabled by default. The database backend is selected at runtime from the connection URL string.

### Sea-query integration: depth

Sea-query is used for **DDL emission only** in migrations. Every `Operation::forwards` and `Operation::backwards` call invokes `sea_query::Table::create()`, `sea_query::Table::alter()`, or `sea_query::Table::drop()`, then calls `database.execute_schema(query)` which builds the SQL string via the appropriate `QueryBuilder` and passes it to sqlx. (`cot/src/db/migrations.rs:490-630`, `cot/src/db/sea_query_db.rs:68-76`)

sea-query is also used for DML queries in the ORM layer (not migration-specific).

sea-schema is **not used**.

---

## Test surface

**Confidence: high**

Migration tests are split between unit and integration:

**Unit tests (SQLite via `TestDatabase`):**

`cot/src/db/migrations.rs:2023-2453` contains inline `#[cot_macros::dbtest]` tests running against an in-memory SQLite database. Key tests:

- `test_migration_engine_run` — applies a single migration, verifies no error
- `test_migration_engine_multiple_migrations_run` — applies two migrations
- `test_operation_create_model`, `test_operation_add_field`, `test_remove_field_operation_forwards` — verify individual DDL operations
- `operation_custom_forwards`, `operation_custom_backwards` — verify custom operations execute

**CLI snapshot tests (no live database):**

`cot-cli/tests/migration_generator.rs` — pure static analysis tests, no DB connection required. Tests include:
- `create_model_state_test` — verifies diff produces correct `CreateModel` operations with correct field metadata
- `create_models_foreign_key` — verifies FK ordering (parent before child)
- `create_models_foreign_key_cycle` — verifies cycle-breaking logic

`cot-cli/tests/snapshot_testing/migration/mod.rs` — end-to-end CLI snapshot tests using `insta` + `assert_cmd_snapshot!`. Tests `cot migration make` and `cot migration new` by creating temp package directories.

**Resource files:** `cot-cli/tests/resources/example_database_model.rs` — a fixture model struct used across snapshot tests.

**No Postgres-specific integration tests** — all runtime DB tests use `TestDatabase` (SQLite in-memory). The postgres backend is never exercised in tests visible in this repo.

---

## Lessons for Djogi

### Adopt

1. **Const-based migration definitions.** cot's use of `const OPERATIONS: &'static [Operation]` means migrations are zero-cost at runtime — no heap allocation for the plan. Djogi's `_up.sql`/`_down.sql` files are the equivalent "compile-time" artifact but stored differently (SQL text vs. Rust constants). The lesson is that the plan should be frozen at author time and immutable at runtime. (`cot/src/db/migrations.rs:1697-1709`)

2. **Embedding model snapshots inside migration files.** cot puts `#[model(model_type = "migration")]` structs directly inside migration files. This is the source for computing "previously migrated state" without reading the live DB schema. Djogi's equivalent is `target/djogi_models.json` — a different artifact form, but the same architectural decision: the diff is against a declared-desired prior state, not live introspection. Both are correct. cot's approach demonstrates this compiles — it is not merely theoretical.

3. **Topological sort with cycle breaking via FK decomposition.** The `remove_cycles` + `toposort_operations` logic in `GeneratedMigration::new` (`cot-cli/src/migration_generator.rs:1058-1115`) handles circular FK references elegantly: extract the FK from the `CreateModel`, emit it as a later `AddField`. Djogi should plan for the same scenario in its diff engine.

4. **App-namespaced table naming.** `{app_name}__{model_name}` (double underscore) prevents table name collisions across apps/crates in a multi-app project. Djogi's single-Postgres-schema approach needs an equivalent — either the double-underscore convention or explicit schema namespacing.

### Reject

1. **No advisory lock.** cot's complete absence of concurrency control is a hard gap vs. Djogi's planned `pg_advisory_lock(...)` via Flyway pattern. cot proves that you can ship a migration system without it, but concurrent deploy scenarios will silently double-apply or interleave migrations. Djogi must not skip this.

2. **No checksum.** cot has no way to detect post-apply mutation of migration files. Djogi's planned `V:hex` Liquibase-style versioned checksum format is essential for trust in the migration ledger. This is a gap cot proves is painful in practice.

3. **No transaction wrapping.** Operations and ledger INSERT are issued as separate statements. A crash between DDL success and ledger INSERT leaves the DB in an applied-but-untracked state. Djogi should use per-migration DDL transactions where the DB supports them.

4. **No destructive classifier.** RemoveField and RemoveModel are generated silently. Djogi's planned Prisma-style two-bucket classifier (warnings vs. unexecutableSteps) is superior for production safety.

5. **`todo!()` for field type changes.** The `make_alter_field_operation` function panics at runtime (`cot-cli/src/migration_generator.rs:835`). This is a fundamental gap — Djogi must handle field type changes (at minimum by generating a `CREATE TABLE + copy + DROP` sequence or explicit `ALTER COLUMN TYPE`).

6. **sqlx as the runtime driver.** Djogi plans `tokio-postgres` + `deadpool-postgres` (explicitly not sqlx). cot is deeply coupled to sqlx via `sea-query-binder`. Djogi's Postgres-only commitment means it can use `tokio-postgres` directly and avoid sqlx's runtime genericity overhead.

### Defer

1. **Multi-app migration support.** cot supports multiple apps each with their own `migrations.rs`, linked via `MigrationDependency`. Djogi's single-crate design can defer this until the multi-crate use case arises. Revisit when Djogi supports workspace-level migration composition.

2. **`cot migration new` (empty custom migration stub).** Useful for hand-crafted data migrations. Djogi should add this after the core generate/run/repair workflow is stable.

3. **Backwards (down) migrations.** cot generates both `forwards` and `backwards` operations for structural changes. Djogi generates paired `_up.sql`/`_down.sql`. The reverse-migration semantics are equivalent in intent. Djogi should verify that remove-field backwards (re-add with what default?) is thought through — cot stores the full field spec in `RemoveModel { fields }` for exactly this purpose.

---

### Direct comparison

| Dimension | cot | Djogi (planned) |
|---|---|---|
| **Descriptor source** | `#[model]` Rust structs scanned from `src/**/*.rs` via `syn::parse_file` at CLI invocation time | `target/djogi_models.json` produced from Rust descriptors (build.rs or CLI step) |
| **Prior-state representation** | Snapshot structs (`#[model(model_type = "migration")]`) embedded inside each migration file | Rendered ledger: applied migrations are re-executed mentally via the SQL files to produce a JSON equivalent (or separate snapshot JSON) |
| **Diff timing** | CLI-time static AST diff, no live DB required | CLI-time JSON diff, no live DB required |
| **Migration output format** | Single `.rs` file per migration (Rust `const` operations + snapshot structs) | Paired `NNNN_name_up.sql` + `NNNN_name_down.sql` files |
| **Numbering scheme** | `m_0001_initial`, `m_0002_auto_YYYYMMDD_HHMMSS`, `m_NNNN_<custom>` | `NNNN_name_up.sql` / `NNNN_name_down.sql` |
| **Ledger table name** | `cot__migrations` | `djogi_migrations` (planned) |
| **Ledger columns** | `id`, `app`, `name`, `applied` (4 columns) | `version`, `description`, `checksum`, `execution_mode`, `out_of_order`, `partial_apply` + more (richer) |
| **Checksum** | None | `V:hex` versioned format (Liquibase pattern) |
| **Lock strategy** | None | `pg_advisory_lock(...)` session-scoped (Flyway pattern) |
| **Transaction model** | None — DDL and ledger INSERT are independent statements | Per-migration DDL transaction where possible |
| **Destructive classifier** | None — silent drop | Two-bucket: warnings vs. unexecutableSteps (Prisma pattern) |
| **Rename handling** | Drop + add (data loss, no heuristic) | Drop + add (planned default; explicit annotation TBD) |
| **Out-of-order policy** | Not configurable; topology sort enforces strict ordering | First-class workflow with flag |
| **Repair / fake / baseline** | None | All three as first-class workflows |
| **Field type change** | `todo!()` panic — not implemented | Must implement |
| **Async executor** | tokio + sqlx | tokio + tokio-postgres + deadpool-postgres |
| **DDL builder** | sea-query (runtime DDL generation from const descriptions) | Raw SQL files (no builder) |
| **Schema introspection** | None (snapshot-based only) | None initially (descriptor-driven only) |
| **CONCURRENTLY support** | None | None initially (Custom migration escape hatch) |
| **Postgres-specific types** | None (ColumnType is cross-DB) | Full Postgres native types (JSONB, arrays, pgvector) |
| **Composite unique / indexes** | Not supported | Planned |
| **Backwards migrations** | Yes — every structural operation has a `backwards` implementation | Yes — `_down.sql` per migration |
| **Multi-app / crate** | Supported via `MigrationDependency::migration(app, name)` | Single-app initially |

### Surprises

1. **`todo!()` panic in `make_alter_field_operation` is shipped in 0.6.0.** The field-type-change path of the diff engine will crash the CLI. This is a documented-by-silence gap: cot cannot generate a migration when you change `i32` to `i64` on an existing field. Users must write a `Custom` migration manually. Djogi must not ship with this gap.

2. **No sea-schema despite sea-query.** Given that cot uses sea-query for DDL, the natural extension would be sea-schema for introspection. cot explicitly does not use it. This validates Djogi's descriptor-only approach — live schema introspection is not required to build a functional migration system.

3. **Snapshot structs inside migration files create a two-column truth.** The migration file contains both the operational plan (`const OPERATIONS`) and the model snapshot (`struct _TodoItem { ... }`). The CLI reads the snapshot to know "what was the model at migration N". This is elegant but means the snapshot must be kept in sync with the operations — if you hand-edit the operations but forget the snapshot, the next `migration make` will generate incorrect diffs. Djogi's external `djogi_models.json` avoids this coupling.

4. **Table naming convention (`app__model`) is load-bearing.** The `{app}__{model}` naming is not merely cosmetic — the CLI uses the crate name as `app`, and the table name is `{crate_name_as_snake_case}__{model_name_as_snake_case}`. In a workspace with multiple apps, each gets its own prefix. Djogi's Postgres-schema approach achieves similar isolation differently, but cot proves the double-underscore convention is sufficient for single-schema deployments.

5. **cot's migration system is tightly coupled to its ORM model trait.** `AppliedMigration` is itself a `#[model]`-annotated struct managed by the ORM layer. The ledger is bootstrapped using the same `Operation::create_model()` mechanism as user migrations. This is elegant dogfooding, but it means the migration system cannot exist without the full ORM stack. Djogi's design is orthogonal — the runner uses raw `tokio-postgres` and the ledger is plain SQL, independent of any ORM.
