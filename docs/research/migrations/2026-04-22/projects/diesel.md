# Diesel

## Metadata
- Clone path: `/home/tarunvir/projects/diesel-reference/`
- Commit SHA inspected: `df1f3ee56d8c8ae17dfab081de36a17668bfb31c`
- Primary language: Rust
- Total LOC of migration-relevant modules (approximate):

| File | LOC |
|---|---|
| `diesel/src/migration/mod.rs` | 227 |
| `diesel_migrations/src/migration_harness.rs` | 296 |
| `diesel_migrations/src/file_based_migrations.rs` | 407 |
| `diesel_migrations/src/embedded_migrations.rs` | 116 |
| `diesel_migrations/src/rust_migrations.rs` | 382 |
| `diesel_migrations/src/errors.rs` | 131 |
| `diesel_migrations/src/lib.rs` | 74 |
| `diesel_migrations/src/combined_migrations.rs` | 77 |
| `diesel_cli/src/migrations/mod.rs` | 701 |
| `diesel_cli/src/migrations/diff_schema.rs` | 902 |
| `diesel_cli/src/database.rs` | 595 |
| `diesel_cli/src/print_schema.rs` | 1373 |
| **Total** | **~5281** |

---

## Architecture

Migration code lives in two crates:

1. **`diesel_migrations/`** — the library crate that embeds into application code. Sub-structure:
   - `src/migration_harness.rs` — `MigrationHarness` trait and its blanket impl on any `Connection`
   - `src/file_based_migrations.rs` — `FileBasedMigrations` source; reads `up.sql`/`down.sql` pairs from disk
   - `src/embedded_migrations.rs` — `EmbeddedMigrations`; `&'static str` pairs baked in at compile time
   - `src/rust_migrations.rs` — `RustMigrationSource`; Rust closures/functions as migrations
   - `src/errors.rs` — `MigrationError` and `RunMigrationsError` enums
   - `migrations_internals/src/lib.rs` — `TomlMetadata`, directory scanning, `version_from_string`
   - `migrations_macros/src/lib.rs` — `embed_migrations!` proc macro (reads filesystem at compile time)

2. **`diesel_cli/src/`** — the CLI binary (`diesel`). Sub-structure:
   - `migrations/mod.rs` — all subcommand dispatch: `Run`, `Revert`, `Redo`, `List`, `Pending`, `Generate`
   - `migrations/diff_schema.rs` — `--diff-schema` code path that diffs `schema.rs` against live DB
   - `print_schema.rs` — `diesel print-schema` introspection (reads DB, emits `schema.rs`)
   - `database.rs` — `schema_table_exists`, `create_schema_table_and_run_migrations_if_needed`

3. **`diesel/src/migration/mod.rs`** — the core traits (`Migration`, `MigrationSource`, `MigrationMetadata`, `MigrationConnection`, `MigrationVersion`) and the canonical `CREATE TABLE` SQL.

---

## State model (source-of-truth)

**Filesystem is canonical.** Each migration is a directory under `migrations/` named `{version}_{name}` containing at minimum `up.sql` (and optionally `down.sql` and `metadata.toml`).

**What is tracked in the database:** Only which versions have been applied — stored in `__diesel_schema_migrations`. No checksum, no execution timestamp beyond `run_on`, no error state, no partial-apply flag.

**What is tracked in the filesystem:** The migration SQL and metadata. There is no snapshot file (like Djogi's `schema_snapshot.json`); instead, `schema.rs` is regenerated from the live DB by `diesel print-schema`.

**Separation of applied-state from execution-history:** None. There is a single `run_on` timestamp column, but it records creation time of the row (i.e. when the migration ran), not a separate history table. Every applied version is exactly one row; reverted versions are deleted from the table (`diesel::delete(... .find(version))` — `diesel_migrations/src/migration_harness.rs:200-203`). There is no audit log of past operations.

**Pending computation (source-confirmed, high):**
`pending_migrations` fetches `applied_migrations()` (a `SELECT version FROM __diesel_schema_migrations ORDER BY version DESC`), builds a `HashMap` of all filesystem migrations, removes applied ones by version key, then sorts the remainder ascending. The sort is lexicographic on the `version` string, which for timestamp-named directories produces chronological order.
`diesel_migrations/src/migration_harness.rs:111-129`

---

## Ledger / history table

**DDL (verbatim, high):**

```sql
CREATE TABLE IF NOT EXISTS __diesel_schema_migrations (
       version VARCHAR(50) PRIMARY KEY NOT NULL,
       run_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
```

Source: `diesel/src/migration/setup_migration_table.sql:1-4`

The same SQL is used for PostgreSQL, MySQL, and SQLite — all three `MigrationConnection` impls delegate to the same `CREATE_MIGRATIONS_TABLE` constant.
Source: `diesel/src/migration/mod.rs:185`, `diesel/src/migration/mod.rs:206-227`

**Column purposes:**
- `version VARCHAR(50) PRIMARY KEY` — the version string extracted from the directory name by splitting on `_` and stripping `-` characters (e.g., `20151219180527_create_users` → version `20151219180527`). Source: `diesel_migrations/migrations_internals/src/lib.rs:71-73`.
- `run_on TIMESTAMP DEFAULT CURRENT_TIMESTAMP` — timestamp the row was inserted (not configurable).

**Indexes:** None beyond the implicit primary key index on `version`.

**Primary key strategy:** Natural key — the version string itself. No surrogate key.

**Insert on apply:**
```rust
diesel::insert_into(__diesel_schema_migrations::table)
    .values(__diesel_schema_migrations::version.eq(migration.name().version().as_owned()))
    .execute(conn)?;
```
Source: `diesel_migrations/src/migration_harness.rs:178-182`

**Delete on revert:**
```rust
diesel::delete(
    __diesel_schema_migrations::table.find(migration.name().version().as_owned()),
)
.execute(conn)?;
```
Source: `diesel_migrations/src/migration_harness.rs:200-203`

**Setup:** `applied_migrations()` calls `setup_database(conn)` which calls `conn.setup()` which executes the `CREATE TABLE IF NOT EXISTS` DDL before every read. Source: `diesel_migrations/src/migration_harness.rs:215-222`, `diesel/src/migration/mod.rs:206-211`.

---

## Execution

### Lock strategy

**No PostgreSQL advisory lock.** There is no `SELECT pg_advisory_lock(...)` or `LOCK TABLE` in the migration path. A search across all `.rs` and `.sql` files in the repository for `advisory_lock`, `pg_advisory`, `pg_try_advisory`, and `LOCK TABLE` returned zero results. Confidence: **high**.

The only lock present is a **filesystem lock on the migrations directory** (a `.diesel_lock` file), acquired exclusively during `diesel migration generate` to prevent concurrent generation of duplicate versions:
```rust
let mut lock = RwLock::new(migration_folder_lock(migrations_folder.clone())?);
let _ = lock.write().map_err(|err| { ... })?;
```
Source: `diesel_cli/src/migrations/mod.rs:268-273`. This is an `fd_lock::RwLock` — an OS-level file lock, not a database lock — and it is scoped to the `Generate` subcommand only, not to `Run` or `Revert`.

**Concurrency posture:** No DB-level serialization. If two processes run `diesel migration run` concurrently, both will read pending migrations and attempt to apply them. The `version` primary key constraint will cause the second INSERT to fail with a duplicate-key error, but this is not handled gracefully — the migration harness propagates the error without distinction from other DB errors.

### Transaction boundaries

**Default: each migration runs in its own transaction.** Source: `diesel_migrations/src/migration_harness.rs:186-189`:
```rust
if migration.metadata().run_in_transaction() {
    self.transaction(apply_migration)?;
} else {
    apply_migration(self)?;
}
```
The INSERT into `__diesel_schema_migrations` happens inside the same `apply_migration` closure as the migration SQL, so both the DDL and the ledger write are committed atomically or both are rolled back.

**Opt-out:** Per-migration transaction opt-out via `metadata.toml` in the migration directory:
```toml
run_in_transaction = false
```
Source: `diesel_migrations/migrations_internals/src/lib.rs:31-42`, `diesel_migrations/src/file_based_migrations.rs:82-93`. For Rust migrations: `RustMigration::without_transaction()`. Source: `diesel_migrations/src/rust_migrations.rs:305-308`.

**`metadata.toml` parsing:** `TomlMetadata::read_from_file` reads the file; if absent, `TomlMetadata::default()` returns `run_in_transaction: true`. Source: `diesel_migrations/migrations_internals/src/lib.rs:36-55`.

**`redo` command transaction:** `redo_migrations` wraps both the revert and re-apply in a single outer transaction when all migrations being redone have `run_in_transaction = true` and the backend is not MySQL. Source: `diesel_cli/src/migrations/mod.rs:681-687`.

### How non-transactional DDL is handled

Diesel surfaces the Postgres error "cannot run inside a transaction block" with a helpful link to the `FileBasedMigrations` docs. Source: `diesel_migrations/src/errors.rs:109-120`. The user must set `run_in_transaction = false` in `metadata.toml` and is advised to split multi-statement migrations if needed. Source: `diesel_migrations/src/file_based_migrations.rs:33-48`.

---

## Recovery

### Checksum algorithm

**None.** There is no checksum field in `__diesel_schema_migrations`, no hashing of migration file content, and no checksum validation on apply or at startup. The only identity comparison is the `version` string extracted from the directory name. A search for `checksum`, `hash`, `sha`, `md5`, and `fingerprint` across all migration-relevant `.rs` files returned zero results. Confidence: **high**.

**Consequence:** If the content of an already-applied `up.sql` is changed on disk, Diesel will not detect the drift. The applied version row stays; the changed SQL is silently ignored on future `run` invocations.

### Repair commands

**None.** Diesel has no `repair`, `baseline`, `stamp`, `fake`, or `mark-as-applied` command. A search across `diesel_cli/src/migrations/mod.rs` for these terms returned zero results. Confidence: **high**.

The `MigrationCommand` enum defines exactly six variants: `Run`, `Revert`, `Redo`, `List`, `Pending`, `Generate`. Source: `diesel_cli/src/migrations/mod.rs:34-192`.

### Baseline / stamp / fake flows

**None.** To "stamp" a database as having all migrations applied without running them, users must manually INSERT rows into `__diesel_schema_migrations`. There is no CLI support for this. Confidence: **high**.

### Partial-apply handling

**No partial-apply state.** There is no "in-progress" or "failed" marker in the ledger. If a migration fails mid-execution outside a transaction, the DB may be in a partially-applied state and the version row is not inserted; the migration will be attempted again on the next `run`. If inside a transaction, the rollback is automatic and nothing is recorded.

### Out-of-order policy

**Silently accepted when all pending are sorted ascending.** `pending_migrations` computes the set-difference of filesystem versions minus applied versions, then sorts the remainder ascending before running. Source: `diesel_migrations/src/migration_harness.rs:115-129`. This means if migration `003` was applied before `002`, running `run` will attempt `002` as if it were new — there is no out-of-order guard, no warning, and no flag to enable/disable this behaviour. Confidence: **high**.

---

## Diff and generation

### Autogen algorithm

**Partial autogen via `--diff-schema`** (experimental, added in Diesel 2.x): `diesel migration generate --diff-schema[=path/to/schema.rs]` parses the current `schema.rs` file (using `syn` to walk macro invocations) and compares it against the live database schema via `infer_schema_internals`. Source: `diesel_cli/src/migrations/diff_schema.rs:28-240`.

The diff produces three variant types:
- `SchemaDiff::CreateTable` — table present in DB but absent in `schema.rs`
- `SchemaDiff::DropTable` — table present in `schema.rs` but absent in DB
- `SchemaDiff::ChangeTable { added_columns, removed_columns, changed_columns }` — table present in both but columns differ

Source: `diesel_cli/src/migrations/diff_schema.rs:320-337`.

The generated SQL uses `ALTER TABLE ... ADD COLUMN`, `ALTER TABLE ... DROP COLUMN`, `CREATE TABLE`, and `DROP TABLE IF EXISTS`. Source: `diesel_cli/src/migrations/diff_schema.rs:689-861`.

**This is explicitly labeled not production-ready:** the comment in `MigrationCommand::Generate` says the generated migrations "are not expected to be perfect." Source: `diesel_cli/src/migrations/mod.rs:127-131`.

**Without `--diff-schema`:** migrations are entirely hand-written. `diesel migration generate` creates empty `up.sql` and `down.sql` stubs with placeholder comments. Source: `diesel_cli/src/migrations/mod.rs:459-477`.

### Rename handling

**None.** The diff algorithm has no rename heuristic. A renamed column is detected as a `removed_column` + `added_column` pair, which will generate `DROP COLUMN` + `ADD COLUMN` SQL — a destructive, data-losing operation. Source: `diesel_cli/src/migrations/diff_schema.rs:244-284`. Confidence: **high**.

### Destructive-operation detection and gating

**None.** The diff algorithm generates `DROP COLUMN` and `DROP TABLE IF EXISTS` SQL without any warning or gate. The comment `// TODO: handle schema?` appears near `generate_drop_table` but no guard is implemented. Source: `diesel_cli/src/migrations/diff_schema.rs:857`. Confidence: **high**.

---

## Schema metadata

### Composite unique constraints

**Not reflected in `schema.rs`.** The `print_schema` / `infer_schema_internals` pipeline queries `information_schema.table_constraints` and `information_schema.key_column_usage` only to resolve foreign keys. Source: `diesel_cli/src/infer_schema_internals/information_schema.rs:66-104`. Unique constraints are not surfaced as `schema.rs` constructs — `schema.rs` only contains `diesel::table!` macro invocations listing columns with their types, and `joinable!` for foreign keys.

### Composite indexes

**Not reflected in `schema.rs`.** The `diesel::table!` macro does not have a concept of indexes. Indexes are entirely outside Diesel's schema model; they must be created in manual migration SQL.

### Reflection / introspection capability (`print_schema`)

`diesel print-schema` is Diesel's introspection command. It queries the live database and emits a `schema.rs` file containing typed `diesel::table!` macro invocations for each table. Source: `diesel_cli/src/print_schema.rs:1-12` (header comment).

The `print_schema` output can be configured in `diesel.toml` (`[print_schema]` section) to filter tables, sort columns, add doc comments, and import custom types. Source: `diesel_cli/src/config.rs:17`.

`diesel.toml` can also specify a `file = "src/schema.rs"` path, enabling `diesel migration run` to automatically regenerate `schema.rs` after applying migrations (`regenerate_schema_if_file_specified`). Source: `diesel_cli/src/migrations/mod.rs:208`.

---

## Online-safe / staged migration guidance

**No built-in support.** Diesel has no concept of online-safe or backward-compatible migrations, no multi-step staged patterns, no documentation of expand/contract patterns, and no warnings when generating potentially table-locking DDL.

The only relevant facility is `run_in_transaction = false` in `metadata.toml`, which allows running `CREATE INDEX CONCURRENTLY` or similar Postgres-only online operations. The `metadata.toml` example in the `FileBasedMigrations` docstring explicitly lists creating an index on an existing column as a case requiring `run_in_transaction = false`. Source: `diesel_migrations/src/file_based_migrations.rs:82-93`.

No documentation of online-safe patterns exists in the migration source code. Confidence: **high** (absence confirmed by reading all migration-relevant source).

---

## Rust-specific concerns

### Async model

**Diesel is synchronous.** There is no `async` keyword, no `tokio`, and no `async-std` in `diesel_migrations` or the core `diesel/src/migration/mod.rs`. Source: confirmed by `grep -n "async"` across all migration files returning zero results. Confidence: **high**.

A separate community crate `diesel-async` exists (not in this repository) that provides async wrappers around Diesel connections. That crate would also need to implement `MigrationHarness` separately — the current harness blanket impl is on synchronous `Connection` types only. Source: `diesel_migrations/src/migration_harness.rs:162-165`.

### Type-safety surface (macros generating types from `schema.rs`)

The `diesel::table!` macro (not a proc macro — it is a declarative `macro_rules!` macro in the `diesel` crate) generates:
- A table struct with the Rust-mapped table name
- Column marker types with compile-time SQL type information
- `QueryDsl` impls for building typed queries

These generated types are used directly in application query code. The `schema.rs` file must be kept in sync with the live DB or queries will fail to compile.

`embed_migrations!` is a **proc macro** (`#[proc_macro]`) in the `migrations_macros` crate. It reads the migrations directory at compile time and emits a `&'static [EmbeddedMigration]` array as a `const`. Source: `diesel_migrations/migrations_macros/src/lib.rs:115-121`, `diesel_migrations/migrations_macros/src/embed_migrations.rs:7-24`.

### Macro use

- `diesel::table!` — declarative `macro_rules!` macro that generates typed table/column structs
- `embed_migrations!` — proc macro that embeds SQL file contents as `&'static str` constants at compile time
- `#[derive(Queryable, Insertable, AsChangeset, Identifiable, Associations)]` — proc-macro derives in `diesel_derives/`

### Integration with build scripts / codegen

`embed_migrations!` has a known limitation: the Rust proc-macro API does not support signaling a rebuild on external file changes. If only migration files change (without touching `Cargo.toml` or a `.rs` file that uses the macro), `embed_migrations!` will not re-run. The official workaround is to add a `build.rs`:
```rust
fn main() {
    println!("cargo:rerun-if-changed=path/to/migrations");
}
```
Source: `diesel_migrations/migrations_macros/src/lib.rs:100-113`.

There is no `build.rs`-integrated diffing or model-descriptor pipeline (unlike Djogi's `target/djogi_models.json` → `build.rs` flow). Schema generation (`print-schema`) is a manual CLI step, not a build-time step.

---

## Lessons for Djogi

### Adopt

**1. Paired `up.sql`/`down.sql` per directory, version extracted from directory name prefix.**
Diesel uses `{timestamp}_{name}/up.sql` and `down.sql`. Version is extracted by `path.split('_').next().map(|s| s.replace('-', ""))`. Source: `diesel_migrations/migrations_internals/src/lib.rs:71-73`. Djogi already plans the same paired-file structure. This confirms the pattern is battle-tested and idiomatic.

**2. Same transaction wraps migration SQL and ledger INSERT.**
Diesel atomically commits the DDL and the `__diesel_schema_migrations` INSERT in one transaction. Source: `diesel_migrations/src/migration_harness.rs:176-183`. Djogi should do the same for transactional migrations, ensuring the ledger is never updated if the migration fails.

**3. Per-migration transaction opt-out via metadata.**
`metadata.toml: run_in_transaction = false` enables `CREATE INDEX CONCURRENTLY` and similar non-transactional DDL. Source: `diesel_migrations/src/file_based_migrations.rs:82-93`. Djogi's spec should surface a similar per-migration flag.

**4. `CREATE TABLE IF NOT EXISTS` for the ledger table.**
Diesel's ledger setup is idempotent. Source: `diesel/src/migration/setup_migration_table.sql:1`. Djogi should use `IF NOT EXISTS` for the same reason.

**5. Empty-migration guard.**
`RunMigrationsError::EmptyMigration` is returned if a migration file is empty. Source: `diesel_migrations/src/errors.rs:95`. This is a useful guard against accidental empty stubs reaching production.

### Reject

**1. No advisory lock (or any DB-level serialization).**
Diesel takes no Postgres advisory lock during migration execution. Source: confirmed by grep across all `.rs` and `.sql` files — zero matches for `advisory_lock`, `pg_advisory`, `LOCK TABLE`. Without serialization, two concurrent `run` executions can race; the second will fail on the duplicate `version` primary key constraint with an opaque DB error. Djogi's design uses `pg_advisory_lock(...)` — this is the correct approach and should be retained.

**2. No checksum.**
Diesel stores no hash of migration content. Source: zero matches for `checksum`, `hash`, `sha`, `md5` in migration source. This means silent drift if applied `up.sql` is modified on disk. Djogi's ledger includes checksums — retain this, as it enables drift detection that Diesel entirely lacks.

**3. No baseline / stamp / repair.**
Diesel has no `--fake`, `--baseline`, or `repair` commands. Source: `MigrationCommand` enum has six variants, none of which are repair-related — `diesel_cli/src/migrations/mod.rs:34-192`. Djogi's first-class baseline/stamp/repair support is a genuine improvement over Diesel.

**4. No out-of-order guard.**
Diesel silently applies out-of-order migrations (pending set-difference, sorted ascending). Source: `diesel_migrations/src/migration_harness.rs:115-129`. Djogi should retain its explicit out-of-order policy with a per-migration opt-in.

**5. Sync-only `MigrationHarness`.**
The blanket impl of `MigrationHarness` is on synchronous `Connection` types only. Source: `diesel_migrations/src/migration_harness.rs:162-165`. Djogi uses `tokio-postgres` / `deadpool-postgres` and must design its runner around async from the start.

### Defer

**1. `--diff-schema` style autogen from a typed schema file.**
Diesel's `diff_schema.rs` parses `schema.rs` (a Rust file with `diesel::table!` macros) against the live DB to generate migration SQL. Source: `diesel_cli/src/migrations/diff_schema.rs:28-240`. Djogi's model is different (descriptors → JSON → `build.rs`), but the principle of diffing a desired-state representation against the live DB is applicable. Revisit once Djogi's descriptor → SQL translation layer is stable.

**2. `embed_migrations!` — compile-time migration embedding.**
Useful for shipping single binaries or testing with in-memory DBs. Source: `diesel_migrations/migrations_macros/src/lib.rs:74-95`. Defer until Djogi has a stable migration directory layout; requires solving the `build.rs` rebuild signal problem (documented at `diesel_migrations/migrations_macros/src/lib.rs:100-113`).

**3. `RustMigrationSource` — Rust closures as migrations.**
`diesel_migrations/src/rust_migrations.rs` allows registering Rust functions as migrations. Useful for data migrations that require typed application logic. Defer — Djogi's current scope is SQL-only migrations; Rust migrations can be added as a feature-flag extension later.

### Surprises

**S1: The filesystem lock is on the migrations directory, not the database.**
The `.diesel_lock` file (`fd_lock::RwLock`) is acquired only during `migrate generate`, not during `migrate run` or `migrate revert`. Source: `diesel_cli/src/migrations/mod.rs:268-273`, `350-367`. This means concurrent `run` invocations have no serialization at all — only the DB's primary key constraint provides any protection, and it provides it by failing loudly rather than queuing. This contradicts a possible assumption that Diesel uses the filesystem lock for all migration operations.

**S2: The `00000000000000` sentinel version.**
`HarnessWithOutput` suppresses output for migrations with version `"00000000000000"`. Source: `diesel_migrations/src/migration_harness.rs:271, 282`. This is the version of the initial setup migration generated by `diesel database setup` (the `diesel_manage_updated_at` helpers). The sentinel is undocumented in the migration library itself; it is baked into the initial setup SQL path.

**S3: `--diff-schema` generates `DROP COLUMN` and `DROP TABLE` unconditionally without any destructive-change warning.**
Source: `diesel_cli/src/migrations/diff_schema.rs:707-722` (`generate_drop_column`), `851-862` (`generate_drop_table`). The comment `// TODO: handle schema?` near `generate_drop_table` signals this is acknowledged but unimplemented. Djogi's destructive-change classifier is a real differentiator — Diesel's autogen provides no guard here.

**S4: Composite foreign keys are explicitly rejected by `--diff-schema`.**
The diff algorithm returns `Err(UnsupportedFeature("Tables with composite foreign keys are not supported by --diff-schema"))` when it encounters a composite FK. Source: `diesel_cli/src/migrations/diff_schema.rs:146-150`. Djogi's differ should handle composite FKs from the start.

**S5: Diesel's `schema.rs` does not model indexes or unique constraints.**
Neither `print-schema` nor `--diff-schema` surfaces secondary indexes or unique constraints. Source: `diesel_cli/src/infer_schema_internals/information_schema.rs:66-104` (only PK and FK queries); `diesel_cli/src/print_schema.rs` (no index emission). This means index management is entirely outside Diesel's migration model and must be hand-written. Djogi's composite unique constraint and index reflection (if planned) is a genuine gap vs. Diesel.
