# refinery

## Metadata

- **Clone path:** `/home/tarunvir/projects/refinery-reference/`
- **Commit SHA:** `c4f819bbbab3f67c98b4ff44a40cd83430f1172d` ("Bump rand from 0.9.2 to 0.9.4 (#422)")
- **Primary language:** Rust
- **Relevant LOC:** ~9 102 total Rust lines across all crates; core migration logic concentrated in ~1 794 lines across `refinery_core/src/runner.rs`, `traits/`, `util.rs`, `drivers/`, and `refinery_macros/src/lib.rs`
- **License:** MIT (or Apache-2.0 for `refinery-core`)
- **Version:** 0.9.0 (workspace)
- **MSRV:** Rust 1.85 (`Cargo.toml:12`)

---

## Architecture

### Workspace layout

The repository is a Cargo workspace with four members (`Cargo.toml:3-8`):

| Crate | Role |
|---|---|
| `refinery` | Public-facing facade; re-exports `refinery_core` types and `embed_migrations!` macro |
| `refinery_core` | All migration logic: runner, traits, driver impls, config |
| `refinery_macros` | Proc-macro crate providing `embed_migrations!` |
| `refinery_cli` | Binary CLI tool (`refinery migrate` / `refinery setup`) |

### Key files and their roles

- `refinery_core/src/runner.rs` — `Migration`, `Runner`, `RunIterator`, `Target`, `Report` structs; checksum computation
- `refinery_core/src/traits/mod.rs` — `verify_migrations`, DDL constants for ledger table creation and queries, `insert_migration_query`
- `refinery_core/src/traits/sync.rs` — `Transaction` / `Query<T>` / `Migrate` traits (sync path)
- `refinery_core/src/traits/async.rs` — `AsyncTransaction` / `AsyncQuery<T>` / `AsyncMigrate` traits (async path)
- `refinery_core/src/drivers/` — one file per database driver, each implementing the sync or async trait for that driver's concrete connection type
- `refinery_core/src/util.rs` — file-name parsing regex, `find_migration_files`, `load_sql_migrations`, `SchemaVersion` type alias
- `refinery_macros/src/lib.rs` — `embed_migrations!` proc-macro expansion
- `refinery_cli/src/migrate.rs` — CLI `migrate` sub-command; reads SQL files from disk, builds `Runner`, calls `Runner::run` or `Runner::run_async`

### How `embed_migrations!` works at a high level

The macro runs **at compile time** inside `refinery_macros/src/lib.rs:98-155`:

1. Resolves the migrations directory relative to `CARGO_MANIFEST_DIR` (the crate root at compile time).
2. Calls `find_migration_files(location, MigrationType::All)` — a walkdir scan that filters file names against the regex `^([U|V])(\d+(?:\.\d+)?)__(\w+)\.(rs|sql)$`.
3. For `.sql` files: emits `include_str!(path)` — content is baked into the binary at compile time.
4. For `.rs` files: emits the source as an inline `mod` and also `include_str!` (to trigger recompilation on change).
5. Generates a `pub mod migrations { pub fn runner() -> Runner { ... } }` module that constructs `Migration::unapplied(name, sql)` for each file and hands them to `Runner::new(&migrations)`.

**Confidence: high** (read source)

Discovery order is determined by the walkdir filesystem iteration, then sorted by version number in `Runner::run` → `verify_migrations` → `migrations.sort()` (`traits/mod.rs:20`). Checksums are computed at the `Migration::unapplied` call site, which runs at the time the binary first executes `runner()`, not at compile time. Paths and file content are resolved at compile time via `include_str!`.

---

## State model (source-of-truth)

**Confidence: high**

There are two sources:

1. **Migrations on disk / compiled-in:** The list produced by `embed_migrations!` (compile-time discovery) or `load_sql_migrations` (runtime filesystem scan). Either way, these become `Vec<Migration>` passed to `Runner::new`.
2. **DB ledger (`refinery_schema_history`):** Populated by prior runs; queried at the start of each run.

Neither is unconditionally canonical. At run time, `verify_migrations` (`traits/mod.rs:14-93`) reconciles them:

- For every **applied** migration in the DB, it checks whether a matching migration exists on disk with the same version, name, and checksum. If not, `abort_divergent` / `abort_missing` control whether to error or log-and-continue.
- For every **disk** migration not in the DB, if its version is less than or equal to the highest applied version it is treated as missing (same `abort_missing` gate).
- The **DB ledger** determines "current version" — the highest applied version (`traits/mod.rs:53-63`).

The runner applies only migrations with version > current, in ascending version order. Unversioned (`U`-prefix) migrations bypass the ordering check and are always applied if absent from the DB (`traits/mod.rs:76-85`).

---

## Ledger / history table

**Confidence: high** (verbatim DDL from source)

### DDL (verbatim)

```sql
CREATE TABLE IF NOT EXISTS %MIGRATION_TABLE_NAME%(
         version %VERSION_TYPE% PRIMARY KEY,
         name VARCHAR(255),
         applied_on VARCHAR(255),
         checksum VARCHAR(255));
```

Source: `refinery_core/src/traits/mod.rs:107-112`

`%VERSION_TYPE%` resolves to `int4` (default) or `int8` (with `int8-versions` feature flag), controlled at compile time (`traits/mod.rs:118-124`).

### INSERT query (verbatim)

```rust
format!(
    "INSERT INTO {} (version, name, applied_on, checksum) VALUES ({}, '{}', '{}', '{}')",
    migration_table_name,
    migration.version(),
    migration.name(),
    migration.applied_on().unwrap().format(&Rfc3339).unwrap(),
    migration.checksum()
)
```

Source: `refinery_core/src/traits/mod.rs:95-105`

### Columns and their purposes

| Column | SQL type | Purpose |
|---|---|---|
| `version` | `int4` (or `int8`) PRIMARY KEY | Migration version number parsed from filename |
| `name` | `VARCHAR(255)` | Migration name parsed from filename (e.g. `initial`) |
| `applied_on` | `VARCHAR(255)` | RFC 3339 timestamp string of when migration was applied |
| `checksum` | `VARCHAR(255)` | u64 SipHash-1-3 value stored as decimal string |

**Notable absences:** No `success` flag, no `execution_time_ms`, no `out_of_order` marker, no `partial_apply` marker. The schema is minimalist — if a row exists, the migration was applied (no failure state is ever written).

### Table naming

Default name: `"refinery_schema_history"` (`traits/mod.rs:134`).

Configurable via `Runner::set_migration_table_name(&mut self, name: S)` (`runner.rs:339-349`) and CLI `--table-name` flag (`cli.rs:46-48`). The method panics if an empty string is provided. Schema-qualification (e.g. `myschema.refinery_schema_history`) is not handled by any special logic — callers can pass a dot-qualified string, but there is no documentation or test coverage for this.

---

## Execution

### Lock strategy: NONE

**Confidence: high (proved by grep)**

```
grep -rn "advisory\|pg_advisory\|LOCK TABLE\|FOR UPDATE" /home/tarunvir/projects/refinery-reference/ --include="*.rs" --include="*.sql"
```
This grep returns **zero results**. refinery has no advisory lock, no table lock, and no filesystem lock. There is no documented concurrency guarantee whatsoever. Two concurrent runners can race, potentially double-inserting or double-executing migrations if both pass the "no migrations to apply" check before either commits.

Djogi Q1 answer: refinery does NOT use `pg_advisory_lock`. It provides no equivalent mechanism. The concurrency gap is not documented in the README or code comments.

### Transaction boundaries

**Confidence: high**

Default mode (`grouped = false`): each migration SQL and its corresponding ledger INSERT are executed in **separate, sequential transactions**. The sync path's `Transaction::execute` wraps every call to the database in a fresh transaction (`drivers/postgres.rs:40-47`, `drivers/tokio_postgres.rs:42-49`, `drivers/rusqlite.rs:39-45`). For the default (non-grouped) path in sync (`traits/sync.rs:85-99`), the migration SQL and the ledger INSERT are sent as **two separate `execute` calls**, meaning each gets its own transaction. This means a crash between the migration SQL commit and the ledger INSERT will leave the migration applied but unrecorded.

Grouped mode (`set_grouped(true)`): all migration SQLs and their ledger INSERTs are concatenated and executed in a single `execute` call, which the driver wraps in one transaction. This mode is noted as unreliable on MySQL (`runner.rs:265-268`).

Djogi Q4 answer: refinery does NOT auto-detect `BEGIN`/`COMMIT` in user SQL. No opt-out mechanism for non-transactional DDL (e.g., `CREATE INDEX CONCURRENTLY`) exists. Running such statements inside refinery's auto-transaction will fail at the Postgres level with "ERROR: CREATE INDEX CONCURRENTLY cannot run inside a transaction block."

### Non-transactional DDL handling

No built-in support. There is no annotation, config flag, or auto-detection for `CREATE INDEX CONCURRENTLY` or other non-transactional DDL. Users must work around this by splitting such statements into separate migrations and accepting the race risk in default (non-grouped) mode.

### Concurrency posture

**Confidence: high**

Completely unguarded. Two runners starting simultaneously will both call `assert_migrations_table` (CREATE TABLE IF NOT EXISTS — safe), both query applied migrations (race-free read), both compute the same pending set, and both attempt to execute and insert. Because the ledger `version` column is a PRIMARY KEY, the second inserter will get a unique constraint violation — but only **after** the migration SQL has already been executed twice. This is a silent data-hazard in default mode, and a transaction-level error in grouped mode.

### Sync vs async

refinery supports both sync and async execution via separate trait hierarchies:

- **Sync:** `Transaction` + `Query<T>` + `Migrate` — implemented for `postgres::Client`, `rusqlite::Connection`, `mysql::Conn`
- **Async:** `AsyncTransaction` + `AsyncQuery<T>` + `AsyncMigrate` — implemented for `tokio_postgres::Client`, `mysql_async::Pool`, `tiberius::Client<S>`

Feature-gated via Cargo features (`refinery-core/Cargo.toml`):

```
rusqlite, postgres, postgres-tls, tokio-postgres, tokio-postgres-tls,
mysql, mysql_async, tiberius, tiberius-config
```

**Deadpool integration** is documented in the README: `pool.get().await?` yields a `ClientWrapper`; calling `.deref_mut().deref_mut()` produces the underlying `tokio_postgres::Client`, which implements `AsyncMigrate`. No native deadpool trait impl exists — this is a workaround through deref coercions (`README.md:72-77`).

**sqlx is explicitly not supported.** The README notes: "If you are using a driver that is not yet supported, namely SQLx you can run migrations providing a Config instead" (`README.md:27`). The `Config` struct itself implements the driver traits for supported backends.

---

## Recovery

### Checksum

**Confidence: high**

Algorithm: **SipHash-1-3** (`SipHasher13` from the `siphasher` crate).

What is hashed (in order, `runner.rs:92-96`):

```rust
let mut hasher = SipHasher13::new();
name.hash(&mut hasher);       // migration name string (e.g. "initial")
version.hash(&mut hasher);    // migration version integer
sql.hash(&mut hasher);        // full SQL content as &str
let checksum = hasher.finish(); // u64
```

The checksum is stored as a **plain decimal u64 string** in `VARCHAR(255)`. No version prefix (unlike Liquibase's `V:hex` format). No normalization of line endings — `sql.hash(&mut hasher)` hashes the raw `&str` as provided by `include_str!` or `fs::read_to_string`. On Windows, `include_str!` normalizes `\r\n` to `\n`, but `fs::read_to_string` does not. This means a migration file with Windows line endings will produce a different checksum when loaded from the CLI versus a previously recorded checksum from an embedded binary compiled on Linux. This is a silent drift risk when teams mix OSes.

The `siphasher` crate comment in source explicitly states the motivation (`runner.rs:84-91`):
> Previously, `std::collections::hash_map::DefaultHasher` was used [...] However, that implementation is not guaranteed [...] We now explicitly use SipHasher13 to both remain compatible with existing migrations and prevent breaking from possible future changes to `DefaultHasher`.

Djogi Q3 answer: no checksum format versioning prefix; no line-ending normalization. Checksum covers name + version + SQL — not just SQL. Prisma's checksum only covers SQL content; refinery's inclusion of `name` and `version` means a file rename changes the checksum even if SQL is unchanged.

### Repair / stamp / baseline / fake

**Confidence: high**

| Workflow | Exists? | Details |
|---|---|---|
| **Fake** (stamp) | Yes | `Target::Fake` / `Target::FakeVersion(v)` — records migrations as applied without executing SQL (`runner.rs:47-51`). CLI flag: `-f` |
| **Repair** | No | No mechanism to rewrite ledger checksums |
| **Baseline** | No | No way to mark all existing migrations as pre-applied from a snapshot |
| **Rollback/undo** | No | Explicitly rejected by design; README directs users to write a new forward migration (`README.md:103-107`) |

Djogi Q7 answer: if a hand-fixed production ledger diverges from disk (e.g., checksum mismatch after emergency SQL edit), the only remedy is `set_abort_divergent(false)` to log-and-continue, or manually updating the ledger row. There is no `repair` command.

### Partial-apply handling

No partial-apply state is tracked. In default (non-grouped) mode, the migration SQL and ledger INSERT are in separate transactions. A crash or connection loss after the migration SQL commits but before the ledger INSERT will leave the schema changed with no ledger record. On restart, refinery will attempt to re-run the migration (since it is not in the ledger), likely producing a SQL error on the already-applied DDL.

### Out-of-order policy

**Confidence: high**

Strictly controlled by the `abort_missing` flag. By default (`abort_missing = true`), any versioned migration found on disk with version ≤ current DB version but not in the ledger causes an error (`traits/mod.rs:76-82`). With `abort_missing = false`, the runner logs an error but continues.

Unversioned (`U`-prefix) migrations are exempt: `verify_migrations` explicitly notes they bypass the out-of-order check (`traits/mod.rs:292-313` unit test: `verify_migrations_checks_unversioned_out_of_order_doesnt_fail`). A `U`-prefix migration will be applied regardless of its version number relative to the current DB version, as long as it is not already in the ledger.

Djogi Q8 answer: strict version ordering for `V`-prefix by default; configurable via `abort_missing`. `U`-prefix provides a gap-tolerant escape hatch. Gap tolerance for `V`-prefix requires disabling `abort_missing` globally (no per-migration opt-in).

---

## Diff and generation

**Confidence: high**

refinery does **not** autogenerate migrations from any schema model. It is a pure migration runner. Users write `.sql` files or Rust functions returning SQL strings. No model diffing, no schema introspection, no file generation.

The README mentions `Barrel` as a complementary crate for schema generation from Rust code, but refinery itself has no integration beyond accepting the `String` that a Barrel migration returns.

---

## Schema metadata

**Confidence: high**

None. refinery has no awareness of:

- Composite unique constraints
- Composite indexes
- Schema reflection / introspection
- Column types, nullability, or defaults

This is entirely out of scope for a migration runner.

---

## Online-safe / staged migration guidance

**Confidence: high**

No built-in support, no documented patterns, no warnings about `CREATE INDEX CONCURRENTLY`, lock contention, or zero-downtime techniques. The README's "Rollback" section is the only advisory content, and it only addresses rollback philosophy, not online safety.

---

## Rust-specific concerns

### Async model

Async support is gated behind the `tokio-postgres` or `mysql_async` or `tiberius` features. The `AsyncMigrate` trait uses `async_trait` proc-macro (`traits/async.rs:11-12`). The tokio runtime is required for the tiberius path in the CLI (`migrate.rs:68-75`); library users supply their own runtime.

### Type-safety surface

Minimal. Migration versions are `i32` (`SchemaVersion = i32`) or `i64` (with `int8-versions` feature). SQL is always `String` / `&str` — no compile-time SQL parsing. The `embed_migrations!` macro provides compile-time file discovery but not compile-time SQL validity checking.

### Macro use (`embed_migrations!`)

The macro produces a `pub mod migrations` containing:
- Optionally one `pub mod V{n}__{name}` per `.rs` migration, inlining its source.
- A `pub fn runner() -> Runner` that instantiates all migrations and returns a `Runner`.
- Optionally a `pub enum EmbeddedMigration` (with `enums` feature) mapping versions to typed enum variants.

The macro panics at compile time if file names do not match the naming convention. It reads SQL content via `include_str!`, baking it into the binary. **Discovery and path resolution happen at compile time; checksum computation happens at runtime** (when `runner()` is called and `Migration::unapplied` is invoked).

### Feature flags for backend selection

From `refinery-core/Cargo.toml`:

```toml
rusqlite          # sqlite via rusqlite (sync)
rusqlite-bundled  # rusqlite with bundled SQLite
postgres          # postgres crate (sync)
postgres-tls      # postgres + native-tls
tokio-postgres    # tokio-postgres (async)
tokio-postgres-tls
mysql             # mysql crate (sync)
mysql_async       # mysql_async (async)
tiberius          # SQL Server via tiberius (async)
tiberius-config   # tiberius + config struct
int8-versions     # use i64 for version numbers
enums             # generate EmbeddedMigration enum
config            # enable Config struct and file-based setup
toml              # enable TOML config file parsing
serde             # enable serde derives on public types
```

### Driver abstraction design

**Confidence: high**

Two separate trait hierarchies gated by feature flags:

**Sync path:**
```rust
pub trait Transaction {
    type Error: std::error::Error + Send + Sync + 'static;
    fn execute<'a, T: Iterator<Item = &'a str>>(&mut self, queries: T) -> Result<usize, Self::Error>;
}
pub trait Query<T>: Transaction {
    fn query(&mut self, query: &str) -> Result<T, Self::Error>;
}
pub trait Migrate: Query<Vec<Migration>> where Self: Sized { /* default methods */ }
```
Source: `traits/sync.rs:8-19`, `105-199`

**Async path:**
```rust
#[async_trait]
pub trait AsyncTransaction {
    type Error: std::error::Error + Send + Sync + 'static;
    async fn execute<'a, T: Iterator<Item = &'a str> + Send>(&mut self, queries: T) -> Result<usize, Self::Error>;
}
#[async_trait]
pub trait AsyncQuery<T>: AsyncTransaction {
    async fn query(&mut self, query: &str) -> Result<T, Self::Error>;
}
pub trait AsyncMigrate: AsyncQuery<Vec<Migration>> where Self: Sized { /* default methods */ }
```
Source: `traits/async.rs:11-25`, `126-205`

Each driver file (`drivers/postgres.rs`, `drivers/tokio_postgres.rs`, etc.) implements these traits for one concrete connection type. All migration logic lives in the trait default methods — drivers only implement the two primitive operations (`execute` and `query`). This is the key extensibility mechanism: any type implementing `Transaction + Query<Vec<Migration>>` gets `Migrate` for free.

Djogi Q5 answer: driver abstraction is trait-based (`Transaction` / `AsyncTransaction`) combined with Cargo feature flags. The trait boundary is minimal — two methods, one for writes and one for reads. There is no runtime driver registration.

---

## Lessons for Djogi

### Adopt

- **`U`-prefix for unversioned (out-of-order) migrations** (`util.rs:15`, `runner.rs:18-21`): The `V`/`U` prefix distinction is a clean way to expose configurable out-of-order semantics without a global flag. Djogi could adopt a similar prefix convention for migration files that are explicitly intended to be replayed without strict ordering (e.g., idempotent seed data).
  Citation: `refinery_core/src/runner.rs:18-21`, `traits/mod.rs:76-85`

- **Separated `Transaction` / `Query` primitives** (`traits/sync.rs:8-19`): Splitting write and read operations into separate trait bounds keeps the driver interface minimal. Djogi's `tokio-postgres` adapter can adopt the same pattern: implement a thin `execute` wrapper that handles `pg_advisory_lock` + transaction lifecycle, leaving all migration logic in trait defaults.
  Citation: `refinery_core/src/traits/sync.rs:8-19`

- **`Target::Fake` / `Target::FakeVersion`** (`runner.rs:44-50`): Fake-apply is the minimum viable "stamp" workflow needed when bootstrapping a new runner against a database that already has schema applied manually. Djogi's design spec calls this "baseline/fake" and should implement it with the same semantics — record without executing.
  Citation: `refinery_core/src/runner.rs:44-50`

- **Report struct returning applied migrations** (`runner.rs:203-218`): Returning a `Report` with the list of applied `Migration` structs from `Runner::run` gives callers structured feedback without them having to re-query the ledger. Djogi should do the same.
  Citation: `refinery_core/src/runner.rs:203-218`

- **Explicit `SipHasher13` instead of `DefaultHasher`** (`runner.rs:84-96`): The comment at `runner.rs:84-91` explains exactly why `DefaultHasher` is unsuitable for migration checksums across Rust versions. Djogi must not use `DefaultHasher`. However, Djogi should use a content-only hash (SHA-256 of SQL) rather than a composite hash of name + version + SQL, to allow safe file renames.
  Citation: `refinery_core/src/runner.rs:84-96`

### Reject

- **No advisory lock** (proved by grep, zero hits for `pg_advisory_lock` / `advisory` / `LOCK TABLE`): Djogi's design requires `pg_advisory_lock(...)` for session-scoped concurrency safety. refinery's absence of any lock means concurrent runners will silently double-execute migrations in the pathological case. Djogi must not inherit this design.

- **Ledger INSERT outside the migration transaction** (`traits/sync.rs:85-99`): In default (non-grouped) mode, refinery sends the migration SQL and the ledger INSERT as two separate `execute` calls, each wrapped in its own transaction. A crash between them orphans the migration. Djogi should always write the ledger row atomically with the migration SQL in a single transaction, or prove it cannot with the affected DDL class (CONCURRENTLY).
  Citation: `refinery_core/src/traits/sync.rs:85-99`

- **Checksum over name + version + SQL** (`runner.rs:92-96`): Including `name` and `version` in the checksum means a file rename or version number change breaks the checksum even if SQL is identical. Djogi's `V:hex` checksum format should hash only the SQL content (normalized for line endings).
  Citation: `refinery_core/src/runner.rs:92-96`

- **No line-ending normalization**: `sql.hash(&mut hasher)` hashes raw bytes. Cross-OS development teams will see checksum drift between Windows (`\r\n`) and Linux (`\n`) builds in CLI mode. Djogi must normalize SQL to `\n` before hashing.

- **`VARCHAR(255)` for `applied_on` and `checksum`** (`traits/mod.rs:107-112`): Storing timestamps as RFC 3339 strings and checksums as decimal strings in `VARCHAR(255)` loses type fidelity. Djogi's ledger should use `TIMESTAMPTZ` for `applied_on` and a proper numeric type or `TEXT` with a format-versioned prefix for checksum.

### Defer

- **`enums` feature (typed `EmbeddedMigration`)** (`refinery_macros/src/lib.rs:36-83`): The optional `enums` feature generates a Rust enum with one variant per migration, enabling exhaustive pattern matching over the migration set. Interesting for typed codegen systems, but Djogi's Phase 7 design does not yet need it. Revisit if Djogi adds a compile-time migration manifest for IDE tooling.

- **`RunIterator`** (`runner.rs:398-455`): The `run_iter` method allows callers to apply migrations one at a time, streaming results through a Rust `Iterator`. Useful for progress reporting in long-running batches. Djogi should defer this until a UI integration story exists.

### Surprises

1. **Transaction atomicity is weaker than it appears.** refinery wraps each query in a transaction at the driver level, but the migration SQL and the ledger INSERT are separate transactions in default mode. Most users assume "each migration is atomic" means the ledger write is included. It is not. This is subtler than Flyway's behavior (Flyway writes the ledger row inside the same transaction as the migration SQL, or explicitly outside for non-transactional migrations). Djogi's spec is correct to require a single transaction per migration that includes both the DDL and the ledger INSERT.

2. **Fake-apply does not return applied migrations in `Report`.** With `Target::Fake`, `applied_migrations` in the returned `Report` is empty (`traits/sync.rs:47-51`). The ledger is written, but the report is silent. This is unexpected for callers trying to log what was "applied." Djogi should return the set of stamped migrations even for fake runs.

3. **No deadpool trait impl — deref workaround is the documented pattern.** The README example for deadpool explicitly instructs users to call `.deref_mut().deref_mut()` (`README.md:75`). This is fragile and depends on internal deadpool wrapper types. Djogi uses `deadpool-postgres` directly and should implement `AsyncMigrate` (or its equivalent) on the pool's managed client type, not rely on deref chains.

4. **`int8-versions` flag changes DDL and breaks checksums on migration.** The README warns: "Migrating an existing database's `refinery_schema_history` table to use `int8` versions will break the checksums on all previously-applied migrations" (`README.md:43`). The reason: `version` is included in the checksum hash, and changing the column type also changes how the value serializes through Rust's `Hash` trait (i32 vs i64 produce different hash inputs). Djogi should pick a fixed version column type from day one (`INT8` / `BIGINT`) and never change it.

5. **The `U`-prefix (unversioned) migration type is more powerful than it looks.** Unversioned migrations skip the "missing version" check entirely — they will be applied whenever they are not in the ledger, regardless of their numeric version relative to the current high-water mark. A `U0__seed_data.sql` will always run after version 10 if it was never recorded. This is closer to Flyway's "repeatable migrations" than to its "versioned migrations." Djogi's current spec has no equivalent; this could be a useful escape hatch for idempotent data loads.

6. **No CLI `repair` command despite Flyway inspiration.** The README explicitly cites Flyway as the design inspiration (`README.md:105`). Flyway has `repair` as a first-class command. refinery has no equivalent. For a team that edits a migration after deployment, the only option is `set_abort_divergent(false)` (global, noisy) or manual SQL on the ledger table.

7. **sqlx is explicitly unsupported.** This is notable because sqlx is the dominant async SQL crate in the Rust ecosystem and has its own migration system. refinery's README acknowledges sqlx by name and offers no path other than using the `Config`-based driver (which still requires one of the supported native drivers underneath). Djogi correctly targets `tokio-postgres` directly and avoids this limitation.
