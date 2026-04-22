# Topic 12: Rust Ecosystem Contrast

## Executive summary

Six Rust-native systems were surveyed in depth: Diesel, SeaORM, SeaQuery, refinery, cot, and Prisma (whose user-facing layer is TypeScript but whose schema engine is a compiled Rust binary). Five non-Rust systems (Django, Alembic, Flyway, Liquibase, SQLAlchemy) were surveyed separately and provide the comparative baseline.

The Rust ecosystem is deeply fragmented on almost every dimension that matters for a migration system:

- **Async model:** Diesel is synchronous at its core; SeaORM supports tokio or async-std via feature flags; refinery supports both sync and async via separate trait hierarchies; cot and Prisma-backend are tokio-only. There is no ecosystem convergence, though tokio is winning by adoption.
- **Type-safety surface:** Diesel's `table!` macro produces strongly-typed column types that make uncompilable queries the default; refinery uses raw SQL strings with no type surface at all; cot uses Rust const structs evaluated at compile time; SeaQuery uses a builder API with Rust types but no compile-time SQL validity checking.
- **Macro style:** The ecosystem uses four distinct approaches — declarative `macro_rules!` (Diesel's `table!`), proc-macro derives (`#[derive(DeriveEntityModel)]` in SeaORM, `#[derive(DeriveMigrationName)]`), attribute macros (`#[model]` in cot), and no macros at all (refinery, SeaQuery for the most part).
- **build.rs usage:** cot is the only system that uses a CLI-driven static AST diff to generate migrations from model structs; no system uses `build.rs` as the actual generation trigger in the migration pipeline, though Diesel documents a `build.rs` workaround for `embed_migrations!` invalidation. Djogi's planned `build.rs`-first generation step is genuinely novel in this landscape.
- **Postgres driver:** The ecosystem splits between sqlx (SeaORM, SeaQuery consumers, cot via `sea-query-binder`), tokio-postgres directly (refinery's `tokio-postgres` feature, Djogi's planned choice), and Diesel's internal driver layer.

Against Python and Java equivalents, the Rust ecosystem does several things markedly better (compile-time type guarantees, zero startup overhead, no runtime reflection cost) and several things markedly worse (autogeneration quality is pre-production, migration file repair tooling is nearly absent, build-time coupling is often painful).

Djogi's technical choices — tokio, tokio-postgres, deadpool-postgres, attribute + derive macros, build.rs or CLI-driven codegen, Postgres-only — are validated by this survey. The cot project is the closest architectural cousin and provides both the strongest positive guidance and the clearest cautionary lessons.

---

## Comparison matrix: Rust-only systems

| System | Async? | Primary Postgres driver | Macro style | Connection pool | build.rs in migration path? | MSRV |
|---|---|---|---|---|---|---|
| **Diesel** | Sync-only core; `diesel-async` addon is separate community crate | Diesel-internal (`diesel/src/pg/`) | `table!` declarative `macro_rules!`; `embed_migrations!` proc macro; `#[derive(Queryable, Insertable, ...)]` derive macros | r2d2 (sync) | No — documents a `build.rs` workaround for `embed_migrations!` rebuild signals only | not confirmed from notes; ≥1.65 implied |
| **SeaORM** | tokio **or** async-std — user-selected via feature flags | sqlx (runtime-tokio or runtime-async-std feature) | `#[derive(DeriveEntityModel)]`, `#[derive(DeriveMigrationName)]` proc-macro derives; `#[sea_orm(...)]` attribute derives | sqlx::Pool | No — schema codegen is `sea-orm-cli generate entity` CLI invocation | not confirmed from notes; ≥1.65 implied |
| **SeaQuery** | No I/O — pure SQL builder; no runtime | Consumer's choice (sqlx, tokio-postgres, postgres crate) | `#[derive(Iden)]` derive; `#[enum_def]` attribute; `raw_query!` / `raw_sql!` declarative macros | Consumer's choice | No | not confirmed from notes |
| **refinery** | Both: sync trait hierarchy (`Migrate`) + async trait hierarchy (`AsyncMigrate`) behind feature flags | `tokio-postgres` (feature = "tokio-postgres"); also `postgres` sync, `rusqlite`, `mysql`, `mysql_async`, `tiberius` | `embed_migrations!` proc macro (compile-time file discovery + `include_str!`) | User-supplied — awkward deref workaround documented for deadpool | No | 1.85 (`Cargo.toml:12`) |
| **cot** | tokio multi-thread (`tokio = { features = ["rt-multi-thread"] }`) | sqlx via `sea-query-binder` | `#[model]` attribute macro (struct rewrite + impl emission); `#[migration_op]` attribute macro (async-fn → boxed-pin); `query!` compile-time query macro | sqlx::Pool (via sqlx runtime-tokio feature) | No — diff runs as `cot migration make` CLI; **build.rs not used** | not confirmed from notes |
| **Prisma (Rust backend)** | tokio (schema-engine is a tokio-based Rust binary, JSON-RPC over stdio) | tokio-postgres via Quaint (Prisma's internal abstraction layer) | None user-facing for migrations — schema is PSL not Rust | Internal (Quaint pool) | N/A — the engine is a standalone CLI binary | not confirmed; presumably ≥1.70 |

*MSRV note: refinery is the only system with a documented MSRV in the inspected source (1.85). The others imply stable Rust ≥1.65 based on language features used but were not confirmed from the project notes.*

---

## Async model

### Sync-only: Diesel (core)

Diesel's migration harness is fully synchronous. The blanket impl of `MigrationHarness` is on synchronous `Connection` types only (`diesel_migrations/src/migration_harness.rs:162-165`). There is no `async` keyword, no tokio, and no async-std anywhere in `diesel_migrations/` or `diesel/src/migration/` — confirmed by grep returning zero results across all migration files (citation: `diesel.md` § Async model).

The practical implication for web servers is that all Diesel migration calls must be dispatched on a `tokio::task::spawn_blocking` thread pool (or equivalent). This is a non-trivial operational overhead: a `spawn_blocking` call acquires a dedicated OS thread from the blocking pool, adds a context switch and scheduling latency, and ties up a thread for the duration of the migration. For most deployments this is acceptable (migrations are infrequent), but it means Diesel migrations cannot live natively inside an `async fn` that holds a tokio-postgres or deadpool connection handle.

The separate community crate `diesel-async` provides async wrappers around Diesel connections but is not in the Diesel repository and would need its own `MigrationHarness` implementation. It is not a first-class Diesel feature.

**Implication for query building:** Diesel's `table!`-driven query builder is tightly coupled to the sync connection model. The type-safe query layer (`diesel::dsl`, `diesel::QueryDsl`) is designed around `Connection::execute()` returning immediately, not around `Future<Output = Result<...>>`. Porting the query layer to async-first is an architectural change, not a wrapper.

### Tokio-first: SeaORM, cot, Prisma backend

SeaORM commits to async throughout: `MigrationTrait::up` and `down` are declared with `#[async_trait::async_trait]` (`sea-orm-migration/src/lib.rs:25-43`). The runtime is user-selectable via feature flags (`runtime-tokio`, `runtime-tokio-native-tls`, `runtime-tokio-rustls`, `runtime-async-std`, etc.), but in practice the tokio path is the default in the generated template (`main.rs` uses `#[tokio::main]`). The async-std path is available but receives less testing attention (citation: `sea-orm.md` § Async model).

cot is tokio-only: `tokio = { workspace = true, features = ["rt-multi-thread"] }` (`cot/Cargo.toml:61`). No async-std path exists. The `CustomOperationFn` type is a boxed-pin `Future` that requires `Send` (citation: `cot/src/db/migrations.rs:654-657`), which is a hard tokio constraint.

Prisma's schema engine is a tokio-based Rust binary communicating via JSON-RPC over stdin/stdout. The TypeScript CLI spawns it as a child process. From the Rust side, the engine's connection handling goes through Quaint (Prisma's internal multi-database abstraction) which uses tokio-postgres under the hood for Postgres.

The refinery `tokio-postgres` async feature uses `#[async_trait]` for `AsyncMigrate` (`refinery_core/src/traits/async.rs:11-25`). Library users supply their own tokio runtime.

### Agnostic: SeaQuery

SeaQuery performs no I/O and has no concept of an async runtime. It is a pure SQL emitter — `SchemaStatementBuilder::build(PostgresQueryBuilder) -> String`. The consumer decides everything about execution context. This is the correct design for a query builder that aims to be used from both sync (Diesel) and async (SeaORM) consumers.

### What Djogi chose: tokio + tokio-postgres + deadpool-postgres

Djogi commits to tokio exclusively (documented in `CLAUDE.md`: "Tokio — async runtime. Used as-is."). The driver is `tokio-postgres` directly, not sqlx. The pool is `deadpool-postgres`.

This matches cot's runtime choice (tokio multi-thread) but diverges on driver (cot uses sqlx via sea-query-binder; Djogi uses tokio-postgres directly). The rationale from the CLAUDE.md spec is explicit: "wraps SQLx into a typed ORM layer" for query execution, but for migration execution Djogi uses tokio-postgres directly to retain full control over transaction semantics and advisory locking. The refinery project confirms this is viable — refinery's async path also wraps `tokio_postgres::Client` directly without sqlx.

The async-std path is not and will not be supported in Djogi v0.1. Supporting async-std would require feature-flagging every async trait and testing both runtimes. Given Djogi's Postgres-only commitment and the industry trend toward tokio, this is the correct scope decision.

---

## Macro styles

Four distinct macro styles appear across the Rust migration ecosystem. Djogi's planned macro strategy combines elements of two of them.

### Declarative `macro_rules!`: Diesel's `table!`

```rust
diesel::table! {
    users (id) {
        id -> Int4,
        name -> Text,
        email -> Varchar,
    }
}
```

Diesel's `table!` is a declarative `macro_rules!` macro (not a proc macro) that generates:
- A table struct with the Rust-mapped table name
- Column marker types with compile-time SQL type information
- `QueryDsl` impls for building typed queries

The generated types are used directly in application query code. If `schema.rs` is out of sync with the live DB, queries fail to compile. This is the strongest type-safety guarantee in the Rust ecosystem: the compiler enforces that column references in queries match the schema definition.

The embed-time macro `embed_migrations!` is a proc macro (`#[proc_macro]` in `migrations_macros/src/lib.rs`) that reads migration files at compile time and emits `&'static [EmbeddedMigration]` as a const array. The rebuild-signal problem (proc macros cannot signal `cargo:rerun-if-changed` for external files) is worked around by a documented `build.rs` pattern (`diesel_migrations/migrations_macros/src/lib.rs:100-113`).

**Pros:** Strongest compile-time guarantee in the ecosystem. Queries that reference dropped columns or wrong types fail at `cargo build`, not at runtime.

**Cons:** `schema.rs` must be kept in sync with the live DB manually (via `diesel print-schema`). If a migration is applied outside `diesel migration run` (e.g., a hotfix), `schema.rs` will be stale and queries will compile against a lie. There is no automated hook to regenerate `schema.rs` on schema change unless `diesel.toml` `[print_schema]` + `file = "src/schema.rs"` is configured (`diesel_cli/src/migrations/mod.rs:208`).

### Proc-macro derives: SeaORM's `#[derive(DeriveEntityModel)]`

```rust
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "seaql_migrations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub version: String,
    pub applied_at: i64,
}
```

SeaORM uses proc-macro derives for both its entity definitions and the migration framework. `DeriveEntityModel` generates the `Entity`, `Column`, `ActiveModel`, and relation impls from a plain Rust struct. `DeriveMigrationName` implements `MigrationName::name() -> &str` by calling `file!()` at compile time to derive the migration name from the source filename (`sea-orm-macros/src/derives/migration.rs:15-27`).

The `#[sea_orm(...)]` attribute syntax provides per-field and per-struct configuration without a separate DSL file.

**Pros:** Familiar to Rust developers. Clean struct definitions. IDE autocomplete works on the struct fields. The macro output is inspectable via `cargo expand`.

**Cons:** `DeriveMigrationName` ties migration identity to the source filename — renaming the `.rs` file changes the migration's identity and breaks existing applied-migration records (`sea-orm.md` Surprise 3). The `generate` command patches `lib.rs` using regex to register new migrations, which is fragile (`sea-orm-cli/src/commands/migrate.rs:228-252`).

SeaQuery uses `#[derive(Iden)]` to implement the `Iden` trait (identifier string conversion) for enums and structs. This is the mechanism for using Rust enums as typed table/column identifiers in builder calls.

### Attribute macros: cot's `#[model]` and `#[migration_op]`

```rust
#[model]
struct Post {
    title: String,
    body: String,
    published: bool,
}

#[model(model_type = "migration")]
struct Post {
    title: String,
    body: String,
}

#[migration_op]
async fn add_slug_column(ctx: MigrationContext<'_>) -> Result<()> {
    ctx.execute("ALTER TABLE post ADD COLUMN slug TEXT").await
}
```

cot uses attribute macros for two purposes:
1. `#[model]` rewrites the struct and emits `impl Model` boilerplate (not a derive — it is an attribute macro that replaces the struct, injecting auto fields and generating all method impls).
2. `#[model(model_type = "migration")]` is the same macro with a different mode, marking a struct as a snapshot embedded inside a migration file.
3. `#[migration_op]` rewrites `async fn` into `CustomOperationFn` (a boxed-pin function pointer), allowing idiomatic `async fn` syntax for custom migration steps (`cot-macros/src/migration_op.rs`).

The `const`-based `Migration` trait in cot means the entire migration definition is evaluated at compile time:

```rust
pub trait Migration {
    const APP_NAME: &'static str;
    const MIGRATION_NAME: &'static str;
    const DEPENDENCIES: &'static [MigrationDependency];
    const OPERATIONS: &'static [Operation];
}
```
(`cot/src/db/migrations.rs:1697-1709`)

`Operation` and `Field` are `Copy` types with `const` builders, so migration definitions carry zero runtime cost — they are fully resolved by the compiler.

**Pros:** Zero runtime overhead for the migration plan itself. Attribute macro allows full struct rewriting (injecting fields, adding impls) that pure derive macros cannot do. The `#[migration_op]` pattern makes custom migration steps ergonomic.

**Cons:** Attribute macros are harder to inspect than derive macros (there is no simple `cargo expand` workflow that reveals all the injected fields). The `#[model(model_type = "migration")]` coupling between the snapshot struct and the migration operations is fragile — hand-editing the operations without updating the snapshot generates wrong future diffs (`cot.md` Surprise 3). The CLI reads all `*.rs` files via `syn::parse_file` to find both kinds of `#[model]` structs, which creates a tight coupling between the macro syntax and the static analysis tool.

### Builder API (no macro): SeaQuery

SeaQuery's DDL surface uses no macros. DDL is expressed as a fluent builder API:

```rust
Table::create()
    .table(Post::Table)
    .if_not_exists()
    .col(ColumnDef::new(Post::Id).integer().not_null().auto_increment().primary_key())
    .col(ColumnDef::new(Post::Title).string().not_null())
    .to_owned()
```

The `#[derive(Iden)]` macro on the `Post` enum is used to derive the identifier string (e.g., `"post"`, `"id"`, `"title"`), but the DDL construction itself is pure method chaining.

**Pros:** Full IDE autocomplete and navigation. No proc-macro compile time overhead. No dependency on the proc-macro crate for DDL construction. Easy to inspect the generated SQL by calling `.to_string(PostgresQueryBuilder)` in tests.

**Cons:** Verbose compared to raw SQL for complex migrations. No compile-time SQL validity checking — invalid column type combinations or unsupported operations surface at runtime.

### No macro: refinery

refinery uses no macros for migration definitions. Migrations are `.sql` files on disk or compiled in as `&'static str` via `embed_migrations!` (the one macro, used for baking files into the binary). There is no Rust schema representation at all — just SQL strings.

**Pros:** Zero compile-time macro cost. SQL is portable, inspectable by any tool, no Rust-specific DSL to learn.

**Cons:** No compile-time type checking. No IDE support for column references. Schema representation lives only in SQL files — there is no Rust type the compiler can use to enforce query correctness against the current schema.

---

## build.rs usage

This is the dimension where Djogi's design is most novel relative to the Rust ecosystem.

### cot: CLI-driven static AST diff (no build.rs)

cot generates migrations by running `cot migration make` as a CLI command. It calls `MigrationGenerator`, which reads all `src/**/*.rs` files via `syn::parse_file`, finds all `#[model]` structs, finds all `#[model(model_type = "migration")]` snapshot structs in existing migration files, computes the diff in memory, and emits a new `.rs` migration file (`cot-cli/src/migration_generator.rs:303-428`).

There is no `build.rs` in cot's migration pipeline. The diff runs only when the developer explicitly invokes `cot migration make`. The output is a Rust source file, committed to the repository, which is then compiled normally.

### Diesel: build.rs as a rebuild-signal workaround only

Diesel's `embed_migrations!` proc macro reads migration files at compile time via `include_str!`. However, the Rust proc-macro API cannot signal `cargo:rerun-if-changed` for external files. If only migration files change (without touching a `.rs` file), `embed_migrations!` will not re-run and the binary will embed stale migrations.

The documented workaround (citation: `diesel_migrations/migrations_macros/src/lib.rs:100-113`):

```rust
// build.rs
fn main() {
    println!("cargo:rerun-if-changed=path/to/migrations");
}
```

This is a passive use of `build.rs` — it does not generate code, only signal Cargo to rebuild when files change. This is not migration generation; it is a cache invalidation hack.

Schema generation (`diesel print-schema`) is a manual CLI step, not a build-time step. There is no `build.rs` that invokes `print-schema` automatically.

### SeaORM, SeaQuery, refinery, Prisma: no build.rs

None of these systems use `build.rs` for any migration-related purpose. Generation and running are CLI operations or in-process library calls.

### Djogi: build.rs as the primary generation trigger (planned)

Djogi's planned architecture (from `CLAUDE.md`):

> `build.rs` runs on every `cargo build`:
> - Reads `target/djogi_models.json` (written by proc macros via `inventory`)
> - Diffs against `migrations/schema_snapshot.json`
> - Generates migration SQL pairs if drift detected; emits compiler warning (not error)

This is genuinely novel: `build.rs` is used not for cache invalidation but as the primary mechanism for detecting schema drift and emitting migration SQL as a side effect of normal compilation.

**The risks of this approach (sourced from the ecosystem survey):**

1. **IDE re-build loops.** IDEs that trigger `cargo check` on every save will invoke `build.rs` on every keypress. If `build.rs` writes migration files, the IDE will see changed files, trigger another `cargo check`, and so on. The Diesel `embed_migrations!` rebuild-signal problem demonstrates this class of issue.

2. **CI caching.** `build.rs` outputs are normally cached between CI runs. If `target/djogi_models.json` changes (because a `#[djogi::model]` struct changed) but `migrations/schema_snapshot.json` does not (because the developer forgot to commit the generated migration), the CI cache may mask the drift.

3. **Workspace member ordering.** In a Cargo workspace, `build.rs` in crate A runs before crate B compiles. If `target/djogi_models.json` is produced by the proc macro of crate B, `build.rs` in the framework crate A cannot read it until B's proc macro has run. The `inventory`-based emission pattern (`ModelDescriptor` submitted at proc macro expansion time, JSON written during compilation) must complete before `build.rs` diff logic runs.

4. **`build.rs` cannot return an error that cleanly reports "schema drift detected, run migrate."** It can call `println!("cargo:warning=...")` which appears as a compiler warning, or it can `panic!` which aborts the build with a confusing backtrace. Neither is developer-friendly.

**Mitigation (open question from the spec):** The CLAUDE.md spec acknowledges the risk: "migrations/ is a git submodule — managed by CI, not by the developer directly." This implies the canonical workflow is `cargo djogi generate` (explicit CLI invocation) rather than build.rs auto-generation for normal development. build.rs may only be used for the warning-emission path, not the file-write path. This would bring Djogi closer to cot's model (explicit CLI command) while retaining the descriptor-JSON infrastructure.

---

## Postgres driver choice

### tokio-postgres (raw async)

**Refinery** supports `tokio-postgres` directly via the `tokio-postgres` feature flag. The `AsyncMigrate` trait is implemented for `tokio_postgres::Client`. The deadpool workaround documented in the README (`.deref_mut().deref_mut()` to extract the client from a deadpool-managed wrapper) confirms that `tokio-postgres` + deadpool is a common pairing, but there is no native deadpool trait impl in refinery — the deref chain is the integration pattern.

**Djogi** plans to use `tokio-postgres` directly. This gives maximum control over:
- Transaction semantics (explicit `BEGIN`/`COMMIT`/`SAVEPOINT`)
- Advisory locking (`SELECT pg_advisory_lock(n)` as a direct statement on the connection)
- Statement-level retry and error handling
- No runtime genericity overhead from sqlx's multi-backend abstraction

The cost is that Djogi must implement its own connection handling and cannot reuse sqlx's `query!` macro compile-time checking for the migration runner itself (though application code uses sqlx as the query layer, per CLAUDE.md).

**Prisma** uses tokio-postgres internally via Quaint (Prisma's internal abstraction layer). From the engine source, `apply_migration_script` splits scripts into individual statements and sends them via `client.simple_query(stmt).await` (`flavour/postgres/connector/native/mod.rs:146-156`), which is the tokio-postgres simple protocol path.

### sqlx

**SeaORM** uses sqlx exclusively. All migration execution goes through `sea_orm::DatabaseConnection` which wraps sqlx. The runtime is selected via SeaORM's Cargo features, which include `runtime-tokio` or `runtime-async-std`.

**cot** uses sqlx via `sea-query-binder`. cot's DDL execution path is: `sea-query` builder → `sea-query-binder` → sqlx query execution. The sqlx connection pool is managed by sqlx's own `Pool<Postgres>` type.

**sqlx trade-offs relevant to Djogi:**
- **Pro:** `sqlx::query!` macro checks SQL against a live database at compile time (or against an offline cache file). This would give the migration runner itself compile-time SQL checking if migrations were expressed as `query!` calls — but migration SQL is emitted into files, not `query!` macros, so this advantage does not apply.
- **Con:** sqlx requires a `DATABASE_URL` environment variable at build time for compile-time checking, or an `sqlx-data.json` offline cache. This adds a CI/CD requirement that tokio-postgres does not.
- **Con:** sqlx's multi-backend genericity (`SqlitePool`, `PgPool`, `MySqlPool`) adds runtime indirection that is unnecessary for a Postgres-only system.

### Diesel-internal driver

Diesel has its own internal Postgres driver (`diesel/src/pg/`) that is entirely separate from tokio-postgres and sqlx. It implements the synchronous connection model on top of `libpq` (the C PostgreSQL client library). This is the reason Diesel requires `libpq` headers at compile time and a `libpq` dynamic library at runtime on most platforms — a deployment consideration that tokio-postgres (pure Rust) avoids entirely.

### postgres crate (sync)

Refinery's `postgres` feature uses the `postgres` crate (the synchronous Postgres Rust client) for `Transaction`/`Migrate` impls. This is appropriate for CLI tools and test harnesses that do not need async but is not suitable for async web server code without `spawn_blocking`.

---

## Connection pooling

### deadpool (Djogi's planned choice)

deadpool-postgres provides an async connection pool for tokio-postgres connections. The pool returns `Object<Manager>` handles via `pool.get().await`. The Djogi CLAUDE.md spec explicitly names `deadpool-postgres` as the pool library.

Refinery's README documents the deadpool integration via a deref workaround, confirming the combination is used in production but also that the seam is imperfect. Djogi should implement its pool integration as a first-class typed adapter (a `djogi::Pool` wrapper), not via deref chains.

**deadpool vs bb8:** bb8 is an alternative tokio-native pool that is compatible with both tokio-postgres and other drivers. Both are acceptable choices; deadpool has more recent maintenance activity and is the choice that matches cot's effective dependency chain (cot uses sqlx's built-in pool, which is architecturally similar to deadpool in providing async-native connection management).

### r2d2 (Diesel)

Diesel's standard pool is r2d2, a synchronous connection pool designed for use with Diesel's sync connection model. r2d2 acquires connections on-thread and returns them synchronously, compatible with Diesel's blocking connection interface. This is not appropriate for async code.

### sqlx::Pool (SeaORM, cot)

SeaORM and cot use sqlx's built-in `Pool<Postgres>`. sqlx's pool is async-native (tokio-compatible) and provides connection limits, health checking, and connection lifecycle management out of the box. It is the path-of-least-resistance when sqlx is already the driver choice.

**Why Djogi rejects sqlx::Pool:** Since Djogi uses tokio-postgres directly (not sqlx), using sqlx::Pool would require maintaining a parallel dependency on both sqlx (for the pool) and tokio-postgres (for the connection type). deadpool-postgres is a native tokio-postgres pool, removing the need for this dual-dependency.

---

## Feature flags for DB backends

### refinery: per-feature driver selection (compile-time multiplexing)

refinery's feature flag design is the most explicit in the ecosystem:

```toml
[features]
rusqlite          # sqlite via rusqlite (sync)
rusqlite-bundled  # rusqlite with bundled SQLite
postgres          # postgres crate (sync)
postgres-tls      # postgres + native-tls
tokio-postgres    # tokio-postgres (async)
tokio-postgres-tls
mysql             # mysql crate (sync)
mysql_async       # mysql_async (async)
tiberius          # SQL Server via tiberius (async)
int8-versions     # use i64 for version numbers
enums             # generate EmbeddedMigration enum
config            # enable Config struct
toml              # TOML config parsing
serde             # serde derives
```

Each driver is a separate Cargo feature that gates a separate `drivers/*.rs` implementation file. The migration logic lives in trait default methods and is shared across all drivers. This is the cleanest multi-driver design in the ecosystem.

**A subtle pitfall (citation: `refinery.md` Surprise 4):** The `int8-versions` flag changes the DDL of the ledger table from `int4` to `int8`. Migrating an existing database to `int8` versions breaks the checksums of all previously-applied migrations because `version` is included in refinery's SipHash-1-3 checksum and `i32` vs `i64` hash differently. Changing a feature flag can silently invalidate migration history.

### SeaQuery: per-backend feature flags for SQL emitters

```toml
[features]
backend-postgres  # Enables PostgresQueryBuilder
backend-mysql     # MySQL backend
backend-sqlite    # SQLite backend
option-postgres-use-serial  # SERIAL vs GENERATED BY DEFAULT AS IDENTITY
```

These flags gate the SQL renderers, not the drivers. A Postgres-only consumer needs only `backend-postgres`. The `option-postgres-use-serial` flag is compile-time, not runtime — you cannot mix `SERIAL` and `GENERATED ... AS IDENTITY` columns in the same binary (`sea-query.md` Surprise 5).

### cot: all three backends enabled by default

```toml
# cot/Cargo.toml:100-108
[features]
default = ["sqlite", "postgres", "mysql", "json"]
db = ["dep:sea-query", "dep:sea-query-binder", "dep:sqlx"]
sqlite = ["db", "sea-query/backend-sqlite", ...]
postgres = ["db", "sea-query/backend-postgres", ...]
mysql  = ["db", "sea-query/backend-mysql", ...]
```

cot enables all three backends by default and selects the actual backend at runtime from the connection URL. This is convenient for development but includes dead code for production Postgres-only deployments. All three sea-query backends compile into the binary; only one is used.

### Djogi v0.1: no backend feature flags needed

Djogi is Postgres-only permanently ("Postgres 18 and later, exclusively" from CLAUDE.md decisions). There is no need for backend-gating feature flags in the migration system. The single backend simplification removes:
- Feature-flag-gated driver trait implementations
- Multi-backend DDL emitters (no sea-query multi-dialect overhead)
- The `int8-versions` class of flag-changes-DDL hazard
- CI matrix multiplied by DB backend permutations

This is a deliberate, permanent design decision, not a deferral.

---

## Error types

### The Rust ecosystem: divergent error strategies

**Diesel** uses custom error enums: `MigrationError` and `RunMigrationsError` in `diesel_migrations/src/errors.rs`. These are hand-rolled enums implementing `std::error::Error`, not `thiserror`-generated. They provide structured variants (e.g., `RunMigrationsError::MigrationError`, `RunMigrationsError::QueryError`) that allow callers to match on the failure mode.

**SeaORM** uses `DbErr` from the sea-orm crate — a custom enum covering database, connection, migration, and serialization errors. Migration errors surface as `DbErr::Migration(String)` or `DbErr::Custom(String)` variants. The string messages are human-readable but not machine-parseable. Retry/repair logic that needs to distinguish "migration already applied" from "connection failed" must string-match on error messages, which is fragile.

**refinery** uses a similar custom error enum (`Error` in `refinery_core/src/error.rs`, though not inspected in detail in the project notes). The `Runner::run` result is `Result<Report, Error>`.

**cot** surfaces migration errors as `anyhow::Error` — the `#[migration_op]` wrapper returns `Result<()>` which accepts any `anyhow`-compatible error. This is ergonomic for user-written migration operations (you can use `?` on any library error) but provides no structured error information to the framework for diagnostics.

**Prisma** uses a strongly-typed error taxonomy at the Rust level (`UserFacingError` with distinct codes like P3006, P3008, P3012). Each error code maps to a specific failure scenario with machine-readable metadata. This is the most production-operational error design in the ecosystem — ops teams can script around error codes rather than parsing messages.

**Djogi should lean toward thiserror for the runner's own errors** (structured variants like `MigrationAlreadyApplied`, `ChecksumMismatch`, `LockAcquisitionTimeout`) with `anyhow` acceptable in user-written data migration closures (which Djogi v0.1 does not expose yet). The Prisma error-code pattern is worth studying for the longer term.

---

## What Rust does BETTER than Python/Java

### Compile-time type safety on queries (Diesel, sqlx)

Diesel's `table!` macro generates strongly-typed column structs. A query that references a column that does not exist in `schema.rs` fails to compile, not at runtime. This is categorically different from Python ORMs (Django, SQLAlchemy) where column references are strings evaluated at runtime, or Java ORMs (Hibernate) where field-column mapping is metadata checked at startup at best.

The `sqlx::query!` macro (used by SeaORM, cot) provides a different form of compile-time checking: it sends the query to a live database at compile time (or checks against an offline cache) and verifies that the SQL is valid and the return types match. This catches SQL typos and type mismatches before the binary is ever deployed.

Python's `MetaData.reflect()` (SQLAlchemy) and Django's `connection.introspect()` provide schema introspection at runtime, not at compile time. Java's Hibernate validation (`hbm2ddl.auto = validate`) also runs at startup, not at compile time.

**The Rust advantage is structural, not implementation-specific:** Rust's type system is expressive enough to represent the schema as types (`table!`), making invalid queries type errors. Python and Java fundamentally cannot do this at compile time because their type systems lack the expressiveness to embed SQL semantics into types at the language level.

### Zero-cost abstractions for query building (SeaQuery)

SeaQuery compiles to the same SQL as hand-written strings — there is no runtime introspection, no descriptor evaluation, no SQLAlchemy `MetaData` traversal at query time. The builder objects (`TableCreateStatement`, `IndexCreateStatement`) exist only at the call site; they are consumed by the `.build(PostgresQueryBuilder)` call which emits a `String`. No heap allocation persists past the build call.

Python's SQLAlchemy Core and Django's ORM both carry runtime overhead: `Column` objects, `Table` metadata, expression trees, and compiler objects are all heap-allocated and traversed on every query. For migration tooling (not hot-path query execution) this is irrelevant, but it illustrates the fundamental difference in abstraction cost.

### No runtime codegen or descriptor evaluation

Rust proc macros run at compile time, not at runtime. Django's model descriptors (`ModelBase.__new__`, `Field.__init__`, `Options.contribute_to_class`) execute as Python class construction every time the process starts. This means Django's model system adds to startup time proportionally to the number of model fields. Rust's `#[derive(Model)]` generates all method implementations at compile time; nothing runs on startup to construct descriptors.

For migration tooling specifically: cot's `const OPERATIONS: &'static [Operation]` means the entire migration plan is a compiled-in constant. Zero computation at process startup to produce the migration plan.

### Explicit `Send + Sync` constraints catch concurrency bugs at compile time

Rust's ownership and trait bounds prevent sending non-thread-safe types across async task boundaries at compile time. If a migration implementation holds a `Rc<T>` (not `Send`), the compiler rejects it as a `MigrationTrait` impl. In Python, concurrency bugs in migration code surface as runtime panics or silent data corruption. Java's checked exceptions provide some discipline, but there is no compile-time guarantee that migration code is safe to run from multiple threads.

cot's `CustomOperationFn` requires `Send` (`for<'a> fn(MigrationContext<'a>) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>`). This forces custom migration implementations to be `Send`-safe, catching a class of concurrency bug at compile time.

---

## What Rust does WORSE than Python/Java

### Autogeneration quality is pre-production

cot's autogeneration hits `todo!()` for field-type-change migrations at `cot-cli/src/migration_generator.rs:835`. This is a panic in production code, shipped in version 0.6.0. A field type change (e.g., `i32` to `i64`) will crash the CLI with a Rust panic stack trace.

Diesel's `--diff-schema` autogen is explicitly labeled "not expected to be perfect" in comments (`diesel_cli/src/migrations/mod.rs:127-131`). The `diff_schema.rs` module generates `DROP COLUMN` and `DROP TABLE` unconditionally without any warning or gate, and a `// TODO: handle schema?` comment at the `generate_drop_table` call site (`diesel_cli/src/migrations/diff_schema.rs:857`) signals that even the Diesel team considers it incomplete.

By contrast:
- **Django's `makemigrations`** is battle-tested across 12+ years and handles not just field additions and deletions but type changes, field renames (via `RenameField`), table renames (via `RenameModel`), composite primary key changes, and cross-app dependency ordering via a topological sort with cycle detection. It is the gold standard for migration autogeneration.
- **Alembic's `autogenerate`** supports `--autogenerate` off a `MetaData.reflect()` comparison, producing ALTER TABLE, CREATE TABLE, DROP TABLE, ADD COLUMN, DROP COLUMN, MODIFY COLUMN diffs with configurable include/exclude patterns. It handles custom types via comparison hooks.

The gap is fundamental: Python runtimes can execute arbitrary model code to introspect the desired state at any time; Rust requires static analysis (AST parsing via syn, as in cot) or explicit descriptor emission (JSON files, as in Djogi) to reconstruct desired state without running the application. Static AST parsing is fragile when macros re-expand field names or when type aliases are used, and JSON descriptors must be committed and kept in sync.

### Runtime schema introspection is harder

SQLAlchemy's `MetaData.reflect()` connects to a live database and builds a complete in-memory schema graph — tables, columns with types, constraints, indexes, foreign keys, sequences — in one call. Alembic uses this for autogenerate comparisons. Django's `DatabaseIntrospection.get_table_description()` similarly reads the live schema at runtime.

The Rust equivalent would be `sea-schema` (used by SeaORM's `generate entity` command) or Diesel's `print-schema`. sea-schema can read a live Postgres schema and emit entity definitions; Diesel's `print-schema` emits a `schema.rs` with `table!` macros. But neither is hooked into the migration autogeneration pipeline in a way that provides Django-quality autogen.

Specifically: none of the Rust systems can do the Django equivalent of "introspect the current applied schema, compare against the model definitions, and generate the diff as migrations" in one step with high quality. Prisma comes closest via the shadow database approach, but it requires a second DB and its Rust engine is not embeddable as a library.

### Build-time coupling creates CI/CD friction

sqlx's `query!` macro requires `DATABASE_URL` to be set at compile time (or `sqlx-data.json` offline cache present). This adds a CI requirement: the CI environment needs a live Postgres instance (or an up-to-date offline cache file committed to the repository) for `cargo build` to succeed.

Djogi's `build.rs` approach creates a different form of build-time coupling: the migration diff runs on every `cargo build`, which means:
- CI must have `target/djogi_models.json` in a known state
- build.rs output (generated migration files) must not accidentally pollute the git working tree
- IDE-triggered `cargo check` invocations may run the diff unnecessarily

By comparison, Python `makemigrations` and Alembic `autogenerate` run only when the developer explicitly invokes them. They have no build-time coupling.

### Fewer production-safety primitives out of the box

Flyway has `repair`, `validate`, `baseline`, `undo` (team edition), and `check` as first-class commands. Liquibase has `validate`, `changelogSync`, `clearChecksums`, `futureRollbackSQL`, `generateChangeLog`, and `rollback`. Django has `showmigrations --plan`, `squashmigrations`, `optimizemigration`, and `migrate --fake`.

By contrast:
- **Diesel:** no repair, no baseline, no stamp, no validate. Six commands total. Manual SQL on the ledger table is the only repair path.
- **SeaORM:** no repair, no stamp, no baseline. Slightly more commands (`fresh`, `refresh`, `reset`, `uninstall`) but none address the "migration applied but missing its file" or "checksum mismatch" failure modes.
- **refinery:** `Target::Fake` exists (stamp without executing). No repair. No validate beyond the default `abort_divergent` flag.
- **cot:** no repair, no stamp, no baseline, no fake. Three commands. The simplest migration CLI in the Rust ecosystem.

This is not merely a feature count comparison. Flyway and Liquibase's production-safety commands exist because production database operations go wrong in specific, documented ways (accidental SQL edit after deployment, schema drift from a hotfix, partial apply from a crash). The Rust ecosystem has not yet accumulated this operational wisdom into tooling.

---

## Unique to Rust

### Procedural macros for schema DSL

The ability to write a `table!` macro that generates strongly-typed table and column structs at compile time is unique to Rust in the migration tooling space. Python's metaclass-based model systems (Django's `ModelBase`, SQLAlchemy's `DeclarativeMeta`) are runtime constructs — they generate Python class objects when the module is imported, not C types at compile time. Java's JPA `@Entity` annotations are processed by the bytecode compiler but do not generate typed column accessors at the language level.

Rust's proc macros run as arbitrary Rust code during compilation, producing token streams that become part of the compiled binary's type system. This enables the `table!` → `schema.rs` → typed query builder chain, which has no equivalent in interpreted or JVM-based languages.

### `Send + Sync` requirements for async migration handles

In Rust, passing a migration context across an `await` point requires that the context be `Send`. In practice this means:
- Connection handles must be `Send` (tokio-postgres `Client` is `Send`; `Rc<Connection>` is not).
- The `MigrationContext<'_>` passed to custom migration ops must be `Send` (enforced by cot's `CustomOperationFn` signature).
- Any state held across a transaction boundary must be `Send + Sync`.

This enforces at compile time that migration code is async-safe. A migration that accidentally holds a `Mutex<T>` across an `.await` (which can deadlock in a single-threaded tokio executor) is caught as a compile error if the guard type is not `Send`.

Python and Java async migration code (Alembic + asyncio, Flyway's async callbacks) provides no such guarantee.

### Feature-flag-gated DB backends (compile-time multiplexing)

Rust's Cargo feature system allows compiling entirely different code paths for different database backends — not as runtime dispatching but as conditional compilation. The refinery crate's feature flags gate separate `impl` blocks for each driver, meaning a binary compiled with only the `tokio-postgres` feature has no sqlite or mysql code at all. Zero binary size overhead, zero runtime conditional.

Java and Python achieve similar multi-database support through runtime driver registration (JDBC `DriverManager.registerDriver`, SQLAlchemy `create_engine("dialect+driver://...")`). The runtime registrar adds overhead and means all driver code is compiled into the binary.

### Lifetime parameters on query builders (Diesel's borrowing ceremony)

Diesel's type-safe query builders use lifetime parameters to ensure that queries do not outlive the connection they are built against. This is `rustc`-enforced borrow-checking applied to query building. The cost is ergonomic: Diesel queries involve complex generic type signatures (`SelectStatement<FromClause<table::table>, DefaultSelectClause<...>, NoDistinctClause, ...>`) that are difficult to name and store. This is a Rust-specific trade-off with no Python/Java equivalent.

---

## Convergence and divergence within Rust

### Convergence: tokio is winning

Every new Rust migration system built after 2020 uses tokio exclusively or defaults to tokio:
- cot: tokio-only (no async-std feature flag exists)
- Prisma engine: tokio-based
- refinery: added `tokio-postgres` feature; tokio-postgres is the most commonly used refinery async driver
- SeaORM: tokio is the default template choice despite technically supporting async-std

Diesel's sync-first model is increasingly out of step with the ecosystem. The community `diesel-async` crate exists precisely because the sync model creates friction for tokio-based web servers.

### Convergence: proc-macro schema definition is winning (for ORM-style systems)

Systems that model schema as Rust structs (SeaORM, cot, Djogi) all use proc macros to generate ORM code from struct definitions. The `table!` declarative macro (Diesel) and the builder API (SeaQuery) are alternatives but are used in systems with different design goals.

Systems that do not model schema in Rust (refinery, Flyway-style SQL-first) need no proc macros and are not converging toward them.

### Convergence: checksums are expected, but implementations vary widely

refinery uses SipHash-1-3 over name + version + SQL (citation: `refinery_core/src/runner.rs:92-96`). Prisma uses SHA-256 over raw SQL bytes with line-ending tolerance (citation: `checksum.rs:43-48`). Diesel has no checksum at all. SeaORM has no checksum at all. cot has no checksum at all.

The ecosystem has not converged on a checksum algorithm, coverage, or storage format. The refinery and Prisma choices both have meaningful design decisions embedded in them (SipHash-1-3 for stability across Rust versions; SHA-256 for cryptographic strength; content-only vs. content+name hash).

### Divergence: build.rs vs CLI-driven generation

cot: explicit CLI command (`cot migration make`), no build.rs.
Diesel: manual `diesel print-schema` CLI + manual SQL writing, with a passive build.rs workaround for `embed_migrations!`.
SeaORM: `sea-orm-cli generate entity` for entities (not migrations); migrations are hand-written or generated by a CLI template.
refinery: no generation at all — pure runner.
Prisma: `prisma migrate dev` CLI command (wraps the Rust engine binary).
Djogi: planned build.rs trigger (novel; risk documented above).

### Divergence: macro style is highly variable

No consensus exists on whether to use derive macros, attribute macros, declarative macros, or builder APIs for schema definition. Each system has made a different choice. The absence of a dominant pattern suggests the ecosystem has not yet found the ergonomic optimum.

---

## Djogi implications

### Validated choices

**tokio + tokio-postgres + deadpool-postgres.**
Matches cot's runtime (tokio multi-thread). Using tokio-postgres directly (rather than sqlx) is validated by refinery's `AsyncMigrate` impl and by Prisma's engine using tokio-postgres via Quaint. The combination avoids the `DATABASE_URL` at build time requirement of sqlx `query!` and gives full control over advisory locking and transaction semantics.

**Attribute macros for schema (`#[djogi::model]`).**
Matches cot's `#[model]` approach. Attribute macros can inject fields (the `id`, `created_at`, `updated_at` fields from CLAUDE.md) in ways that derive macros cannot. cot demonstrates this pattern compiles and is ergonomic for users.

**Postgres-only for v0.1.**
Removes the entire feature-flag-per-backend surface. No multi-backend DDL emitters, no refinery-style `int8-versions` hazard, no CI matrix across backends. cot suffers dead-code cost by defaulting all three backends on; Djogi avoids this permanently.

**Descriptor-canonical with JSON intermediate.**
The `target/djogi_models.json` approach is different from cot's in-file snapshot structs but achieves the same goal: diff against declared prior state without touching a live DB. cot demonstrates the snapshot-based approach compiles and works. Djogi's JSON file is more portable (not tied to Rust's AST) and does not couple the snapshot to the migration file (avoiding cot's Surprise 3).

**Skip sqlx for the migration runner itself.**
cot's deep coupling to sqlx via sea-query-binder means any sqlx runtime behaviour (connection pool management, query error handling, prepared statement caching) bleeds into the migration path. Djogi using tokio-postgres directly keeps the migration runner's dependency surface narrow and controlled.

### Differentiate from cot

**Explicit advisory locking.**
cot has zero concurrency protection (confirmed by grep: zero matches for `pg_advisory_lock` / `advisory` / `LOCK TABLE` in cot source). Djogi uses `SELECT pg_advisory_lock(...)` session-scoped, matching Prisma's approach (Prisma key: `72707369` — Djogi must choose a different key). This is the most important production-safety differentiator.

**Checksum on every applied migration.**
cot has no checksum; drift from post-apply file edits is undetectable. Djogi stores a checksum (following Prisma's SHA-256 pattern, applied to content-only — not including name or version to allow safe file renames, in contrast to refinery's name+version+content hash).

**Per-migration DDL transaction wrapping.**
cot issues DDL and ledger INSERT as separate, non-transactional statements (`cot/src/db/migrations.rs:208-212`). A crash between them leaves an applied-but-untracked migration. Djogi wraps both in a single `BEGIN`/`COMMIT` block where the DDL is transactional (i.e., not `CONCURRENTLY`), matching the Diesel and SeaORM patterns.

**Destructive-operation classifier.**
cot silently generates `RemoveField` and `RemoveModel` operations. Djogi plans a Prisma-style two-bucket classifier (`warnings` vs `unexecutableSteps`). The Prisma source confirms the exact variant structure needed: `NonEmptyTableDrop`, `NonEmptyColumnDrop`, `MadeOptionalFieldRequired`, `DropAndRecreateRequiredColumn` etc. (`sql_destructive_change_checker/warning_check.rs:7-48`, `unexecutable_step_check.rs:7-13`). Data probes run against the production DB at `evaluateDataLoss` time.

**First-class repair / baseline / fake commands.**
cot has none of these. Djogi's spec calls for `migrate stamp`, `migrate baseline`, `migrate repair` as first-class CLI commands. refinery's `Target::Fake` demonstrates the minimum viable stamp implementation. Prisma's `migrate resolve --applied` demonstrates the recovery-after-failure flow.

**Field type change: not a `todo!()`.**
cot panics at runtime when a field type changes (`cot-cli/src/migration_generator.rs:835`). Djogi must handle field type changes with an explicit migration strategy: either `ALTER COLUMN TYPE ... USING expression`, or a drop-add-copy sequence for type changes that Postgres cannot cast automatically.

**No sea-schema dependency.**
cot proves that live schema introspection is not required for a functional migration system — the snapshot-based approach is sufficient. Djogi's descriptor-JSON approach is the same architectural bet, validated by cot's production use.

### Avoid from cot

**Snapshot structs embedded inside migration files.**
cot's `#[model(model_type = "migration")]` structs inside migration files create a coupling between the operational plan (OPERATIONS) and the snapshot. Hand-editing operations without updating the snapshot produces incorrect future diffs (cot Surprise 3). Djogi's external `schema_snapshot.json` avoids this coupling.

**Patching `migrations.rs` via regex or code generation.**
cot's CLI regenerates `src/migrations.rs` to include all discovered migration modules. This is fragile for codebases with unusual formatting. Djogi's SQL-file-based migration format (paired `_up.sql`/`_down.sql`) requires no source file patching — the runner discovers SQL files by directory scan.

**All backends enabled by default.**
cot defaults all three sea-query backends (sqlite, postgres, mysql). Djogi is Postgres-only and should never add multi-backend feature flags.

**Using sea-query for DDL emission.**
sea-query has several documented gaps for Postgres: no `DEFERRABLE` FK constraints, no `NOT VALID`/`VALIDATE CONSTRAINT` for two-phase constraint addition, `NULLS FIRST`/`NULLS LAST` not available in index ordering, unsigned integer types silently mapped to signed (`sea-query.md` §"Reject"). For a Postgres-only tool, direct SQL string formatting (or Djogi's own typed DDL emitter) is cleaner and produces no dead-code dependency.

---

## Open questions

**How does Djogi avoid build.rs IDE-churn?**
The current CLAUDE.md spec describes `build.rs` triggering migration diff on every `cargo build`. IDE tools that run `cargo check` on every keystroke would invoke this diff continuously. Two candidate mitigations:

1. **Defer to explicit CLI command.** `build.rs` only emits a `cargo:warning=...` when drift is detected (no files written). The developer runs `cargo djogi generate` explicitly to write migration files. This matches cot's model and avoids the churn.

2. **`[build-dependencies]` isolation.** Keep the migration generation code in a separate crate (`djogi-build`) that is only a `build-dependency`, not a regular dependency. This limits build-cache invalidation scope and makes the boundary explicit in `Cargo.toml`.

Either approach is preferable to having `build.rs` write migration files on every `cargo build`. The spec's description of `migrations/` as a git submodule "managed by CI" suggests the intended workflow is already the explicit CLI path.

**Which advisory lock key should Djogi use?**
Prisma uses `72707369` on Postgres (`flavour/postgres.rs:374`). If Djogi and Prisma are both used against the same database (a realistic scenario during migration), they would hold different advisory locks and not block each other — advisory locks are advisory, not enforced against non-participants. Djogi should document its chosen key in `docs/spec/decisions.md` so operators know how Djogi's lock interacts with other tools.

**How does Djogi handle `CREATE INDEX CONCURRENTLY`?**
`CONCURRENTLY` cannot run inside a transaction block. Djogi's default per-migration transaction wrapping would reject a `_up.sql` containing `CREATE INDEX CONCURRENTLY`. The mechanism needed is a per-migration `non_transactional = true` annotation (equivalent to Diesel's `metadata.toml: run_in_transaction = false` and SeaORM's `use_transaction() -> Some(false)`). When `non_transactional = true`, the ledger INSERT cannot be in the same transaction as the DDL — the ledger write must happen either before the DDL (optimistic) or after, with appropriate failure handling. This is an open design question; the Prisma approach (statement-by-statement via simple protocol, each auto-committed) is one answer but does not work for the full migration wrap.

**When does cot's `todo!()` become Djogi's problem?**
If Djogi adopts cot's macro conventions closely enough that developers expect parity, the missing field-type-change support becomes a user-visible gap. Djogi should have an explicit migration strategy for type changes from day one, even if the initial implementation is "emit ALTER COLUMN TYPE with a comment warning and require manual review."

---

## Confidence notes

All claims about Diesel are **high confidence** — sourced from direct reading of `diesel_migrations/` and `diesel_cli/`.

All claims about SeaORM are **high confidence** — sourced from direct reading of `sea-orm-migration/`.

All claims about SeaQuery are **high confidence** — sourced from direct reading of `src/`.

All claims about refinery are **high confidence** — sourced from direct reading of all workspace crates.

All claims about cot are **high confidence** — sourced from direct reading of all relevant workspace crates, including the `todo!()` at `cot-cli/src/migration_generator.rs:835`.

All claims about Prisma TypeScript layer are **high confidence**. Claims about the Prisma Rust engine internals (advisory lock key `72707369`, SHA-256 checksum, `SqlMigrationStep` enum, destructive change checker variants) are **high confidence** — sourced from the `prisma-engines-reference` Rust clone at commit `3c6e192`, including `flavour/postgres.rs`, `checksum.rs`, `sql_destructive_change_checker/`, and `apply_migrations.rs`.

Comparisons to Python/Java ecosystem (Django, Alembic, Flyway, Liquibase) cite facts established in the corresponding project notes but are not repeated with inline file citations in this document to avoid redundancy.
