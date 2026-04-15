> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Key Research Areas

---

## 18. Key Research Areas

### Jsonb\<T\> Field Type
- Proc macro generating typed filter accessors for all known fields at every nesting level
- `Jsonb<T>` internal layout: `data: T` + `extra: IndexMap<String, UnknownField>`
- Serde impl: deserialize known fields into `T`, collect remainder into `extra`
- Serialize: merge `data` serialization with `extra` — unknown fields always written back
- Validation: walk the full schema tree before any save, collect all errors with dot-notation paths
- `UnknownField::as_f64()` etc — `Result`-returning, no implicit coercion, clear error variants
- Nested `Jsonb<T>` within `Jsonb<T>` — recursive unknown field handling at each level

### CRUD Logging
- Diffing model fields between pre-save snapshot and current values
- Diffing `Jsonb<T>` fields recursively — walk both `data` and `extra` at each level
- Generating `FieldPath` in dot-notation through arbitrary nesting depth
- `FieldValue::Unknown(UnknownField)` serialization in the JSONB changes column
- Actor attribution: request-context hook design (Axum `Extension` or `State` pattern)
- `CrudLog::objects().json_path_changed("engine.horsepower")` — filtering changes by path in JSONB

### HeeRanjId Integration
- Installing HeeRanjId SQL functions and tables via `cargo djogi init` and `db reset`
- Startup validation: `NODE_ID` env var → `heer_nodes` check → fail fast if missing
- Rust wrappers: `HeerId::generate(&pool)` and `HeerId::generate_many(&pool, n)` calling `generate_id()` / `generate_ids(n)`
- RanjId wrappers: `RanjId::generate(&pool)` calling `generate_ranj_id()`
- `create_with_id()`: explicit ID INSERT with `ON CONFLICT (id) DO NOTHING`
- Shell bindings: synchronous `HeerId::generate()`, `HeerId::generate_many(n)`, and `RanjId::generate()` in Rhai
- HeerId serde impl: `i64` internally, `String` in JSON — no per-field annotation needed
- RanjId serde impl: `Uuid` internally, standard UUID string in JSON
- Migration path: `#[model(pk = "ranjid")]` for models that need higher capacity

### Proc Macro Design
- Struct field injection (`id`, `created_at`, `updated_at`) via `#[derive(Model)]`
- Emitting `ModelDescriptor` via `inventory::submit!` from inside the derive macro
- Side-channel file (`target/djogi_models.json`) for `build.rs` consumption
- Error message quality: `proc-macro-error`, `syn::Error::new_spanned`
- Testing: `trybuild` for compile-fail, `macrotest` for expansion snapshots

### Build.rs Drift Detection
- Reading `target/djogi_models.json` after proc macro expansion
- Timing: ensuring the side-channel file is written before `build.rs` reads it
- Emitting `warning` vs `error` diagnostics from `build.rs`
- Auto-naming generated migrations from detected `SchemaDelta` variants

### QuerySet and Condition Tree
- `AND`/`OR`/`NOT` as a typed `Condition` enum tree
- Translating `Condition` → positional SQL parameters via sqlx `QueryBuilder`
- `prefetch()`: collect PKs from first result, fire one `IN (...)` per relation, stitch back

### ForeignKey Resolved Access
- `ForeignKey<T>` internal layout: `id: i64` + `resolved: Option<Box<T>>`
- Helpful panic message if `.resolved()` called without prior `.prefetch()`

### ManyToMany Trait
- Blanket impl generating named methods from `RELATION` const
- Bidirectional methods from two separate `impl ManyToMany` blocks without collision
- Shell access: Rhai requires concrete types, so each model gets its own registered type

### Shell Transactions
- Holding a single `sqlx::Transaction` across multiple Rhai calls
- Savepoint implementation via `SAVEPOINT name` / `ROLLBACK TO SAVEPOINT name`
- Auto-rollback on shell exit: drop handler or explicit cleanup
- Prompt state indicator for open transactions

### Schema Differ
- Column type equivalence map (Rust type → SQL type → canonical form)
- Rename detection via `#[field(renamed_from)]`
- Destructive operation gating and `--allow-destructive` flag

### Migration Safety
- Postgres advisory locks during migration to prevent concurrent runners
- `--fake` flag: insert into `_sqlx_migrations` without executing SQL
- Checksum verification of already-applied migrations (sqlx handles this natively)

### ConditionBuilder
- Walking the `Condition` enum tree and emitting correct `$n` positional parameters
- `AND`/`OR` grouping with correct SQL parenthesization
- `IN (...)` expansion for `in_list()` with variable-length parameter lists
- JSONB path operator generation for `Jsonb<T>` subfield filters

### Admin Panel (Dioxus)
- Dioxus 0.7 `#[server]` functions sharing Axum `State<PgPool>` with the main app
- `ForeignKey<T>` select dropdowns for large tables — search-as-you-type rather than loading all options upfront
- Serving the Dioxus WASM bundle via `tower_http::services::ServeDir` within the Axum router
- Admin session auth — signed cookie, independent of application auth
- `Jsonb<T>` unknown fields in admin — read-only display with raw JSON view toggle
- Inline pagination and search for large M2M through-tables

### Two-Database Logging Architecture
- Three concurrent connection pool management at startup
- Async write strategy for CRUD log — non-blocking relative to the application request
- `tracing-subscriber` Layer routing spans and events to the event log database
- Severity routing: `WARN`/`ERROR`/`CRITICAL` fanning out to Sentry, OpenTelemetry, Datadog
- `db reset --wipe-crud-logs` / `--wipe-all-logs` guard implementation
