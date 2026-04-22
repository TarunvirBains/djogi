# SeaORM

## Metadata

- **Clone path:** `/home/tarunvir/projects/sea-orm-reference/`
- **Commit SHA inspected:** `3d33b516e969d936a97f2c89d968c269ed3f62c7`
- **Primary language:** Rust
- **Version:** `2.0.0-rc.38` (`sea-orm-migration/Cargo.toml:14`)
- **Total LOC of migration-relevant modules:**
  - `sea-orm-migration/src/` (all files): 2,171 lines
  - `sea-orm-cli/src/` (all files): 1,312 lines (migrate-related subset: `commands/migrate.rs` = 331 lines, `cli.rs` = 463 lines)
  - `sea-orm-macros/src/derives/migration.rs`: 32 lines
  - **Total: ~3,515 lines**

---

## Architecture

**Migration code lives in two main crates:**

1. `sea-orm-migration/` — the runtime migration crate. Exposes `MigrationTrait`, `MigratorTrait`, `SchemaManager`, and the ledger entity. This is what user code depends on.
   - `sea-orm-migration/src/lib.rs` — top-level module, re-exports, defines `MigrationName` and `MigrationTrait`
   - `sea-orm-migration/src/migrator.rs` — `MigratorTrait` (static-dispatch version) and the `Migration` wrapper struct
   - `sea-orm-migration/src/migrator/exec.rs` — all async execution logic: `install`, `uninstall`, `exec_up_with`, `exec_down_with`, `drop_everything`, `get_migration_with_status`, `insert_migration_record`, `delete_migration_record`
   - `sea-orm-migration/src/migrator/queries.rs` — DB-specific introspection queries: `query_tables`, `query_pg_types`, `query_mysql_foreign_keys`, `get_current_schema`
   - `sea-orm-migration/src/migrator/with_self.rs` — `MigratorTraitSelf` (instance-dispatch version, blanket-impl'd over `MigratorTrait`)
   - `sea-orm-migration/src/seaql_migrations.rs` — the entity definition for the ledger table
   - `sea-orm-migration/src/manager.rs` — `SchemaManager`, the helper passed to user `up`/`down` methods; wraps DDL statement execution
   - `sea-orm-migration/src/connection.rs` — thin re-export: `SchemaManagerConnection = DatabaseExecutor`, `IntoSchemaManagerConnection = IntoDatabaseExecutor`
   - `sea-orm-migration/src/schema.rs` — helper column-definition shorthands (e.g. `pk_auto`, `string_uniq`, `table_auto`)
   - `sea-orm-migration/src/cli.rs` — embeds a Clap CLI into the migration crate itself; user's `main.rs` just calls `cli::run_cli(Migrator).await`

2. `sea-orm-cli/` — the `sea-orm-cli` binary used externally.
   - `sea-orm-cli/src/cli.rs` — defines `MigrateSubcommands` enum and `GenerateSubcommands`
   - `sea-orm-cli/src/commands/migrate.rs` — `run_migrate_command` (shells out to `cargo run --manifest-path`), `run_migrate_init`, `run_migrate_generate`

3. `sea-orm-macros/src/derives/migration.rs` — `DeriveMigrationName` proc macro; the only macro in the migration path.

**Relationship to sea-query:** SeaORM's DDL builder (`Table::create()`, `Index::create()`, etc.) is provided by `sea-query` (re-exported through `sea_orm::sea_query`). This research does not deep-dive into sea-query's internals. `sea-schema` is used for introspection queries (`query_tables`, `has_table`, etc.): `sea-orm-migration/Cargo.toml:19-26`.

---

## State model (source-of-truth)

**Canonical source of truth: user Rust code** — specifically the ordered `Vec<Box<dyn MigrationTrait>>` returned by `MigratorTrait::migrations()`. This is the authoritative ordered list. `sea-orm-migration/src/migrator.rs:56`.

**What is tracked where:**

| Location | What is stored |
|---|---|
| Database (`seaql_migrations` table) | `version` (string) + `applied_at` (unix timestamp i64). Only applied migrations. |
| Filesystem (Rust source, `.rs` files) | The actual migration logic (`up`, `down`, `name`). Also the ordered list in `lib.rs`. |
| Memory (runtime) | `Vec<Migration>` with `MigrationStatus` (Pending / Applied), assembled by diffing DB rows against the in-code list. `sea-orm-migration/src/migrator/exec.rs:36-70`. |

**Separation of applied-state from execution-history:** There is none. The ledger records only that a migration was applied, not when it started, whether it was re-run, or what happened in a partial apply. There is no execution history table. `sea-orm-migration/src/seaql_migrations.rs:1-15`.

---

## Ledger / history table

**Table name:** `seaql_migrations` (overridable via `MigratorTrait::migration_table_name()`). `sea-orm-migration/src/migrator.rs:59-61`.

**Entity definition (exact Rust struct used to generate the DDL):**

```rust
// sea-orm-migration/src/seaql_migrations.rs:1-15
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
// One should override the name of migration table via `MigratorTrait::migration_table_name` method
#[sea_orm(table_name = "seaql_migrations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub version: String,
    pub applied_at: i64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
```

**Column purposes:**
- `version` — the migration name string (e.g. `m20220118_000001_create_cake_table`), used as the primary key. `auto_increment = false` means it is not a sequence. `seaql_migrations.rs:7-8`.
- `applied_at` — Unix timestamp in seconds (`i64`), set at the moment the ledger record is inserted after `up()` returns. `sea-orm-migration/src/migrator/exec.rs:196-210`.

**DDL creation method:** Not a raw SQL string. The `install()` function uses `Schema::new(builder).create_table_from_entity(seaql_migrations::Entity)` with `.if_not_exists()`. The actual SQL is emitted by `sea-query`'s schema builder at runtime for the target backend. `sea-orm-migration/src/migrator/exec.rs:72-84`.

**Primary key strategy:** Single-column text PK (`version`). No surrogate key. No sequence. No index beyond the implicit PK index.

**No additional indexes** are created on the ledger table — there is no `applied_at` index, no composite index.

**Confidence:** high (read source).

---

## Execution

### Lock strategy

**No advisory lock. No lock table. No distributed lock of any kind.** Grep across the entire repository for `advisory`, `pg_try_advisory`, `LOCK TABLE`, `pg_advisory` returns zero results in `.rs` files. Confirmed: `sea-orm-migration/src/migrator/exec.rs` (full read) — no lock calls.

**Concurrency posture:** None enforced by the framework. If two processes run `up` simultaneously, both can win the race on the `IF NOT EXISTS` table creation and then both can attempt to apply the same pending migrations. The only protection is that `INSERT INTO seaql_migrations` will fail for the second process if the PK already exists (unique constraint on `version`), but this is not surfaced as a clean error.

### Transaction boundaries

**Default behavior (Postgres):** Each individual migration runs inside its own transaction. `sea-orm-migration/src/migrator/exec.rs:184-189`:

```rust
fn should_use_transaction(migration: &dyn crate::MigrationTrait, backend: DbBackend) -> bool {
    match migration.use_transaction() {
        Some(v) => v,
        None => backend == DbBackend::Postgres,
    }
}
```

So on Postgres, `use_transaction()` defaults to `None` → `true`. On MySQL and SQLite, it defaults to `false`.

**Transaction scope per migration:** Both the migration DDL and the `INSERT INTO seaql_migrations` record are executed within the same transaction. `sea-orm-migration/src/migrator/exec.rs:254-266`:

```rust
if use_txn {
    let transaction = db.begin().await?;
    let txn_manager = SchemaManager::new(&transaction);
    migration.up(&txn_manager).await?;
    // ...
    insert_migration_record(&transaction, migration.name(), migration_table_name.clone()).await?;
    transaction.commit().await?;
}
```

**Opt-out mechanism:** `MigrationTrait::use_transaction()` returns `Option<bool>`. Returning `Some(false)` disables the automatic transaction. `sea-orm-migration/src/lib.rs:40-43`.

**Manual transaction control:** When `use_transaction()` returns `Some(false)`, the user can call `manager.begin()` to get an inner `SchemaManager` backed by an owned transaction, and call `.commit()` explicitly. `sea-orm-migration/src/manager.rs:52-73`. This is tested in `sea-orm-migration/tests/common/migration/m20250101_000002_manual_transaction.rs`.

**Non-transactional DDL:** No special handling. The framework delegates to whatever DDL the user writes. If a migration issues a `CREATE INDEX CONCURRENTLY` (which Postgres cannot run inside a transaction), the user must set `use_transaction() -> Some(false)` and manage boundaries themselves.

### Async model

SeaORM is fully async. `MigrationTrait::up` and `down` are `async fn` via `async_trait`. The runtime is **user-selectable via Cargo feature flags**:

- `runtime-tokio`, `runtime-tokio-native-tls`, `runtime-tokio-rustls` — delegates to tokio
- `runtime-async-std`, `runtime-async-std-native-tls`, `runtime-async-std-rustls` — delegates to async-std

`sea-orm-migration/Cargo.toml:48-76`. The migration crate itself has no `#[tokio::main]` or `#[async_std::main]` at the library level — this is the user's choice in their `main.rs`.

Migration ordering is sequential within a single `exec_up_with` call — it iterates the pending list in order (`for Migration { migration, .. } in pending_migrations`), awaiting each one before proceeding. `sea-orm-migration/src/migrator/exec.rs:243-268`. No parallelism.

---

## Recovery

### Checksum

**None.** The `seaql_migrations` table has no checksum column. The `exec.rs` file contains no hash computation. Confirmed: grep for `checksum`, `CRC`, `sha`, `hash` across all migration source files returns no results related to migration content verification.

### Repair commands

**None exist.** There is no `repair`, `stamp`, `fake`, or `baseline` subcommand in either `MigrateSubcommands` (`sea-orm-cli/src/cli.rs:109-163`) or the embedded CLI (`sea-orm-migration/src/cli.rs:87-101`).

### Baseline / stamp / fake flows

**Not supported.** No code path exists for marking a migration as applied without running it.

### Partial-apply handling

**No explicit handling.** If `up()` panics or returns an error mid-migration, and `use_transaction()` is `true` (the Postgres default), the transaction is rolled back — the DDL changes and the ledger INSERT are both reverted. If `use_transaction()` is `Some(false)` and the migration partially executes before failing, the DDL changes that already executed stay; the ledger record is never written (since `insert_migration_record` is only called after `up()` returns `Ok`). No repair tool exists to fix this state.

### Out-of-order policy

**Not supported, but silently tolerated in one direction.** The code computes the "pending" set as `migration_in_fs - migration_in_db` (a set difference). `sea-orm-migration/src/migrator/exec.rs:51`. The ordering is preserved from the user-provided `Vec` — pending migrations are applied in the order they appear in `migrations()`. There is no timestamp enforcement of ordering at runtime.

If a migration file is present in the DB but missing from the filesystem, the runner emits an error: `"Migration file of version '{missing_migration}' is missing, this migration has been applied but its file is missing"`. `sea-orm-migration/src/migrator/exec.rs:59-66`. This is a hard error returned as `DbErr::Custom`.

Out-of-order application (inserting a new migration between two already-applied ones) is silently permitted at the code level — the new migration would appear as "pending" and be applied. There is no sequence enforcement that would block it.

### Command behaviors

All commands map through `MigratorTrait` or `MigratorTraitSelf` (the two are blanket-equivalent). `sea-orm-migration/src/migrator.rs` and `sea-orm-migration/src/migrator/with_self.rs`.

| Command | What it does | Source |
|---|---|---|
| `up [--num N]` | Calls `exec_up`. Installs ledger table if absent. Applies pending migrations in order, up to `N` (or all if `N` is `None`). | `migrator.rs:189-206`, `exec.rs:222-237` |
| `down [--num N]` | Calls `exec_down`. Applies pending migrations in reverse order. Default `N=1` from CLI. | `migrator.rs:199-206`, `exec.rs:239-253` |
| `status` | Calls `get_migration_with_status`, logs each migration name and its `Pending`/`Applied` status via `tracing::info!`. No return value beyond `Ok(())`. | `migrator.rs:130-143` |
| `fresh` | Calls `exec_fresh`. Drops all tables (and Postgres types) in the current schema, then calls `exec_up`. **Does not call `down()` methods** — it drops tables directly via SQL. | `migrator.rs:146-153`, `exec.rs:209-220` |
| `refresh` | Calls `exec_down(None)` (all migrations) then `exec_up(None)`. Uses `down()` methods. | `migrator.rs:156-164` |
| `reset` | Calls `exec_down(None)` then drops the `seaql_migrations` table. | `migrator.rs:167-175` |
| `uninstall` | Drops the `seaql_migrations` table only. No schema changes. | `migrator.rs:179-186` |
| `init` | Scaffolds a `migration/` crate from templates. No DB interaction. | `sea-orm-cli/src/commands/migrate.rs:82-127` |
| `generate <name>` | Creates a new timestamped `.rs` file from template and patches `lib.rs` to add the `mod` declaration and `Box::new(...)` entry. | `sea-orm-cli/src/commands/migrate.rs:129-256` |

---

## Diff and generation

### Autogen algorithm

**There is no schema-diff to migration autogen.** SeaORM does not compare a current schema to a desired schema and generate migration SQL.

The `sea-orm-cli generate entity` command goes in the **opposite direction**: it introspects a live database and generates Rust entity files. `sea-orm-cli/src/cli.rs:168-394`. This is not a migration generator.

There is a `schema-sync` / `entity-registry` feature (`sea-orm-migration/Cargo.toml:44`, `sea-orm-migration/src/lib.rs` re-exports `schema-sync` through `sea-orm`): `db.get_schema_registry("my_crate::entity::*").sync(db).await`. `sea-orm-reference/src/database/executor.rs:214-220`. This can auto-create tables from entity definitions, but it is documented as a dev/testing tool and is not part of the migration runner pipeline. It has no concept of versioned migrations, no rollback, no ledger.

### Rename handling

**None.** The user writes DDL explicitly. If a column or table is renamed, the user writes an `ALTER TABLE RENAME` inside `up()`. The framework provides no rename detection or heuristics.

### Destructive-operation detection and gating

**None.** `SchemaManager::drop_table`, `drop_index`, `drop_foreign_key`, `drop_type`, `alter_table` are all thin wrappers that execute whatever statement is passed. `sea-orm-migration/src/manager.rs:96-127`. No classification, no warning, no confirmation prompt for destructive operations.

---

## Schema metadata

### Composite unique constraints

User-side: in a `MigrationTrait::up()`, composite unique constraints are expressed via `sea-query`:

```rust
Index::create()
    .unique()
    .name("idx_user_email_name")
    .table(User::Table)
    .col(User::Email)
    .col(User::Name)
    .to_owned()
```

Naming is fully manual. No naming convention is enforced or generated by the framework.

At the entity level, `#[sea_orm(unique)]` applies to a single column. Composite uniques require an explicit `Index::create()` call in the migration. Confirmed from `sea-orm-migration/src/manager.rs:81-83` which only exposes `create_index(stmt: IndexCreateStatement)`.

### Composite indexes

Same mechanism: explicit `Index::create()` in the migration body. Name is user-supplied string. `sea-orm-migration/src/manager.rs:81-83`.

### Reflection / introspection capability

`SchemaManager` exposes three inspection methods:
- `has_table(table)` — delegates to `sea_schema::{postgres,mysql,sqlite}::has_table`. `manager.rs:131-136`.
- `has_column(table, column)` — delegates to `sea_schema::..::has_column`. `manager.rs:138-167`.
- `has_index(table, index)` — delegates to `sea_schema::..::has_index`. `manager.rs:169-199`.

Full schema introspection (column types, constraints, FKs) is available via `sea-schema` in the `generate entity` workflow, but not exposed through `SchemaManager` within migrations.

---

## Online-safe / staged migration guidance

**No built-in support.** The framework provides no primitives for `CREATE INDEX CONCURRENTLY`, lock-timeout setting, or staged expand-contract patterns.

**What is possible via `use_transaction() -> Some(false)`:** A user can disable the automatic transaction for a migration and issue `CREATE INDEX CONCURRENTLY` directly via `manager.execute(Statement::from_string(...))`. No framework guidance or examples for this pattern exist in the source.

No documentation, warnings, or helpers for online-safe migrations exist in the `sea-orm-migration/` codebase.

---

## Rust-specific concerns

### Async model

- `MigrationTrait::up` and `down` are declared with `#[async_trait::async_trait]`. `sea-orm-migration/src/lib.rs:25-43`.
- `MigratorTrait` methods are all `async` (via `async_trait`). `sea-orm-migration/src/migrator.rs:53`.
- Runtime is user-selected via feature flags: tokio or async-std. `sea-orm-migration/Cargo.toml:48-76`.
- The migration template's `main.rs` uses `#[tokio::main]` by default. `sea-orm-cli/template/migration/src/main.rs`.

### Type-safety surface

- `SchemaManager` is generic over the connection type but erased at the boundary via `SchemaManagerConnection = DatabaseExecutor`. `sea-orm-migration/src/connection.rs:1-4`.
- Column definitions use `sea-query` builder types (`ColumnDef`, `TableCreateStatement`, `IndexCreateStatement`). These are typed Rust builder structs, not raw SQL strings, but the type system does not prevent issuing destructive operations.
- The `schema.rs` helper module provides typed shorthands like `pk_auto`, `string_uniq`, `big_integer`, etc. to reduce boilerplate. `sea-orm-migration/src/schema.rs:55-80`.

### Macro use

- `DeriveMigrationName` is a proc macro that implements `MigrationName::name() -> &str` by calling `sea_orm_migration::util::get_file_stem(file!())`. The migration name is derived from the Rust source filename at compile time. `sea-orm-macros/src/derives/migration.rs:15-27`.
- `DeriveEntityModel`, `DeriveRelation`, `EnumIter` are used on the `seaql_migrations` entity but are standard SeaORM entity macros, not migration-specific. `seaql_migrations.rs:3-12`.
- No other proc macros are used in the migration path.

### How user-written migrations are structured

The canonical structure (from the template at `sea-orm-cli/template/migration/src/m20220101_000001_create_table.rs`):

```rust
use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table("post")
                    .if_not_exists()
                    .col(pk_auto("id"))
                    .col(string("title"))
                    .col(string("text"))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table("post").to_owned())
            .await
    }
}
```

- `#[derive(DeriveMigrationName)]` provides the `name()` method from the filename.
- `down()` has a default implementation in the trait that returns `Err(DbErr::Migration("We Don't Do That Here".to_owned()))`, so it is technically optional. `sea-orm-migration/src/lib.rs:31-33`.
- The user registers migrations by listing them in `fn migrations() -> Vec<Box<dyn MigrationTrait>>`. The list ordering is the authoritative ordering. `sea-orm-cli/template/migration/src/lib.rs`.

---

## Lessons for Djogi

### Adopt

1. **Per-migration transaction opt-out via `use_transaction() -> Option<bool>`** (`sea-orm-migration/src/lib.rs:40-43`, `exec.rs:184-189`). SeaORM's three-state design (`None` = backend default, `Some(true)` = always, `Some(false)` = never) is clean. Djogi should support the same pattern for cases like `CREATE INDEX CONCURRENTLY`. Rationale: the "opt-out" path is the mechanism for online-safe DDL and is the minimal hook needed.

2. **DDL + ledger INSERT in the same transaction** (`exec.rs:254-266`). If either fails, both roll back atomically. Djogi already intends this, but the SeaORM implementation is worth adopting as the exact pattern.

3. **`install()` idempotency via `IF NOT EXISTS`** (`exec.rs:72-84`). The ledger table creation is always attempted before any operation, with `if_not_exists()`. This means the runner can be called safely even if the ledger table already exists.

4. **`manager.begin()` / `manager.commit()` for manual sub-transaction control within a single migration** (`manager.rs:52-73`, tested in `tests/common/migration/m20250101_000002_manual_transaction.rs`). This is a clean API for migrations that need to split DDL into multiple transactions without exiting the migration framework.

### Reject

1. **No advisory lock** (confirmed by absence — zero grep matches for `pg_try_advisory`, `advisory`, `LOCK TABLE` in `.rs` files). Djogi's locked decision to take a Postgres advisory lock before migration execution is correct and not contradicted by SeaORM (SeaORM simply doesn't have one, leaving concurrent runs as a user-side problem). Do not copy SeaORM's lockless posture.

2. **No checksum** (`seaql_migrations.rs:6-10` — no checksum column; `exec.rs` — no hash computation). Djogi's spec includes checksums for change detection. SeaORM provides no prior art here to copy; its omission is a known gap in the tool.

3. **No baseline / stamp / repair** (confirmed by absence in `MigrateSubcommands` enum, `sea-orm-cli/src/cli.rs:109-163`). Djogi's spec requires these as first-class operations. SeaORM's omission confirms this is a genuine gap, not a solved problem that Djogi is reinventing.

4. **No out-of-order enforcement** (`exec.rs:50-51` — pure set difference, no timestamp check). Djogi's spec should be explicit about whether out-of-order migrations are permitted or rejected, and SeaORM's silence on the matter is not a model to follow.

5. **`generate entity` as a migration autogen tool** (it is not — it generates Rust entity files from a live DB, not migration SQL from a schema diff). Djogi's `build.rs` diff approach is orthogonal and more appropriate for Djogi's model-first philosophy.

### Defer

1. **`schema-sync` / entity-registry auto-table-creation** (`src/database/executor.rs:214-220`, `sea-orm-migration/Cargo.toml:44`). SeaORM offers `db.get_schema_registry("my_crate::entity::*").sync(db).await` for dev/test environments. Djogi could offer a similar "apply current descriptor state directly, no migration file" mode for development. Defer until migration system is stable and the use case is validated.

2. **`uninstall` command** (`migrator.rs:179-186`). Drops the ledger table without rolling back schema changes. Useful for resetting migration tracking state without destroying schema. Defer until Djogi has baseline/repair functionality so the operation has a coherent place in the workflow.

### Surprises

1. **`fresh` does not call `down()` methods.** `fresh` drops all tables directly via raw SQL introspection (`query_tables`) and `Table::drop().cascade()`. `exec.rs:96-182`. This means `down()` methods are bypassed for `fresh`, which could leave orphaned Postgres types or application-level side effects undone. Djogi's `reset` / `fresh` equivalent should document whether it uses `down()` or raw drops. Raw drops are faster and do not require `down()` to be correct, but they are also more surprising.

2. **`down()` is optional and defaults to an error.** The default implementation returns `Err(DbErr::Migration("We Don't Do That Here".to_owned()))`. `sea-orm-migration/src/lib.rs:31-33`. This means a `MigratorTrait` with no `down()` implementations will hard-error if `reset` or `refresh` is called. This is an interesting stance: it acknowledges that rollback is often impossible in practice but provides the hook. Djogi's spec should take an explicit position rather than silently inheriting this.

3. **Migration name is derived from the Rust source filename at compile time via `file!()`.** `sea-orm-macros/src/derives/migration.rs:22`. This means renaming the source file changes the migration's identity. If a developer renames `m20220118_000001_create_cake_table.rs` to something else, any instance where the old name was applied will now show it as "missing" and emit a hard error. This is fragile. Djogi's migration naming scheme (version + description in the SQL filename) avoids this Rust-specific footgun.

4. **The `generate` command patches `lib.rs` using regex.** `sea-orm-cli/src/commands/migrate.rs:228-252`. This is fragile for real codebases with unusual formatting. Djogi's `build.rs`-driven approach avoids patching source files entirely.

5. **SeaORM 2.0 uses `GENERATED BY DEFAULT AS IDENTITY` instead of `serial` for auto-increment on Postgres.** `sea-orm-reference/CLAUDE.md` notes this as a 2.0 change. This aligns with Postgres best practices. Djogi should consider the same for any auto-increment columns, though Djogi uses HeeRanjId as the default PK rather than serial sequences.
