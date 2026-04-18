> [Back to README](../../ReadMe.MD) | [All Specs](./index.md)

# Resolved Design Decisions

---

## 19. Resolved Design Decisions

| Decision | Resolution |
|---|---|
| Default PK type | HeerId — `BIGINT DEFAULT generate_id()`, database-native, time-ordered |
| HeerId ID pattern — default | `create()` uses `DEFAULT generate_id()` + `RETURNING id` |
| HeerId ID pattern — bulk | `HeerId::generate_many(n)` pre-allocates IDs before bulk INSERT |
| HeerId ID pattern — form | `HeerId::generate()` at form render time; INSERT with `ON CONFLICT DO NOTHING` |
| Serial PKs | Opt-in via `#[model(pk = "serial")]` |
| Node identity | NODE_ID environment variable; validated against heer_nodes at startup |
| HeerId JSON serialization | Always string — enforced by HeerId serde impl, no annotation needed |
| Datetime library | `time` crate — cleaner API, no CVE history |
| Framework fields (`id`, `created_at`, `updated_at`) | Macro-injected — real fields, not written by developer |
| `create()` input | Struct itself — no separate `CreateVehicle` wrapper |
| FK loading strategy | Explicit `.fetch()` or `.prefetch()` — no lazy loading |
| M2M strategy | Explicit through models required; `ManyToMany<T>` trait for convenience methods |
| M2M method naming | Explicit `RELATION` const — auto-pluralization is error-prone |
| M2M directionality | Each direction declared separately with its own `impl ManyToMany` |
| Shell async strategy | Dedicated tokio runtime + `block_on` per call — no `.await` in shell |
| Shell transactions | `begin()` / `commit()` / `rollback()` / `savepoint()` — explicit, always rollback on exit |
| Transaction timeout | Optional, developer-chosen at `begin()` — config `transaction_timeout_default` pre-fills prompt (default: 30m) |
| Power loss / crash | Postgres auto-rollbacks dropped connections — no manual recovery needed |
| Shell error handling | Print one-liner + save full traceback to `.djogi_shell_errors/`; session never unwound |
| Shell error log retention | 1 year default; configurable via `error_log_retention` in `Djogi.toml` |
| Shell verbose mode | `cargo djogi shell --verbose` — prints full tracebacks inline in addition to saving to disk |
| Shell history | `.djogi_history` gitignored; `scripts/` committed and shareable |
| Shell import/export | `.export`, `.import`, `.bookmark` — named Rhai scripts in `scripts/` |
| Shell headless execution | `cargo djogi shell --run scripts/name.rhai` — runs script without entering REPL |
| Query engine | Djogi-owned `ConditionBuilder` wrapping `sqlx::QueryBuilder` — no third-party query builder |
| Raw SQLx escape hatch | Always available — `sqlx::QueryBuilder` directly accessible |
| CRUD log architecture | Separate `myapp_crud_logs` database; per-model mirror tables; async writes |
| Event log architecture | Separate `myapp_event_logs` database; `tracing`-powered; severity-routed |
| Log database lifecycle | `db reset` wipes app DB only; `--wipe-crud-logs` / `--wipe-all-logs` for log DBs |
| Admin panel | Optional HTMX + Askama; `admin` feature flag; mounted at `/_admin/` |
| Admin form generation | Auto-generated from `ModelDescriptor` — zero per-model UI code required |
| Admin validation | Auto-generated from field annotations + optional `impl AdminClean` hook |
| Admin M2M inlines | Auto-rendered from `impl ManyToMany<T>`; paginated, searchable, configurable |
| Admin opt-out | Per-model `#[model(admin = false)]`; per-field `#[field(admin_hidden)]` |
| `Jsonb<T>` in admin | Nested fieldset from schema; unknown fields shown read-only with raw JSON toggle |
| Seed script transactions | `seeds.rhai` runs inside a transaction by default — partial failure rolls back entirely |
| Query builder — application code | Closure-based: `.filter(\|f\| f.field.gte(val))` — typed, composable, idiomatic |
| Query builder — shell / dynamic | Programmatic: `ModelFilter::new().field(Gte(val))` — serializable, no closures needed |
| Dirty tracking | Off by default; opt-in globally via `Djogi.toml` or per-model via `#[model(dirty_tracking)]` |
| FK cascade default | `RESTRICT` — safest Postgres default; overridden per-field with `#[field(on_delete = "...")]` |
| Field rename detection | `#[field(renamed_from = "old_name")]` — differ treats as rename not drop+add |
| Build drift diagnostic | Compiler-style `note` (not error) — migration generated, build continues, developer reviews |
| Migration generation | Automatic via `build.rs` on drift detection — generates pair, build continues |
| `makemigrations` CLI | Retained as manual trigger for `--dry-run`, `--allow-destructive`, custom naming |
| Schema snapshot | Updated only on successful `cargo djogi migrate` — reflects actual DB state, never build state |
| Migrations folder | Git submodule — pipeline-managed, invisible to developer day-to-day |
| Migration down files | Always generated as a pair; data loss on destructive rollback documented in file |
| Database target | Postgres only — permanent decision, not a limitation; enables JSONB, HeeRanjId, advisory locks, transactional DDL, `RETURNING` |
| Dev database reset | `cargo djogi db reset` — gated on `dev_mode = true` + localhost URL + `DJOGI_ENV != production` |
| CLI interface | `cargo djogi` subcommand — installed via `cargo install djogi-cli`, idiomatic Rust toolchain |
| Djogi's scope | Model derivation chain only — does not duplicate Axum, SQLx, HeeRanjId, or Tokio responsibilities |
| Public requirement translation | Private app requirements may inform Djogi, but specs/docs describe them only as product-agnostic framework capabilities |
| Core vs companion crate boundary | Djogi keeps reusable data-layer primitives; domain policy, workflow logic, and specialized integrations belong in app crates or companion crates |
| `Jsonb<T>` field type | `JSONB` column with typed schema, serde deserialization, validator validation, nested schema support |
| Unknown field preservation | Fields not in schema loaded into `extra: IndexMap<String, UnknownField>` — never dropped on save |
| `UnknownField` variants | `String`, `Bool`, `Float`, `Int`, `Null`, `RawJson` — no implicit coercion between types |
| `UnknownFieldError` | `FieldNotFound`, `TypeMismatch`, `NoImplicitCoercion` — all conversions return `Result` |
| Nested `Jsonb<T>` | Fully supported — each nesting level has its own typed schema and unknown field boundary |
| JSONB subfield queries | Typed filter accessors generated per known field using Postgres JSONB path operators |
| CRUD logging | Optional per-model or global; stored in `_djogi_crud_log` table; off by default |
| CRUD log JSON diffing | Dot-notation paths through full `Jsonb<T>` nesting depth including unknown field changes |
| CRUD log actor | Optional `save_with_actor()` or request-context hook; null if not provided |
| Project scaffolding | `cargo djogi new` — scaffolds project and initializes migrations submodule |
