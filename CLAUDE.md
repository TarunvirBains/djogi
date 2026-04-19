# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Project Is

**Djogi** is a Model-first web framework for Rust. Define your data schema as Rust structs, and the framework derives everything else — ORM, migrations, admin UI, audit trail, shell bindings, JSONB schema handling. One definition, full derivation chain. Djogi's core is **web-framework-agnostic**; per-framework integrations (Axum extractors, etc.) ship behind sub-feature flags so adopters pick their HTTP layer.

The `ReadMe.MD` is the project overview. The full specification lives in `docs/spec/` — read the relevant spec doc before implementing any feature. The [implementation plan](docs/spec/implementation-plan.md) sequences the build.

**Current status:** Implementation in progress. The README is the authoritative specification. When there is a conflict between the README and the code, treat the README as the design target unless otherwise instructed.

## Workspace Layout

```
djogi/                  ← this repo — the framework implementation
  djogi/                ← framework library crate
  djogi-macros/         ← proc macro crate (separate crate — required by Rust)
  djogi-cli/            ← cargo djogi binary
  djogi-shell/          ← Rhai engine + model bindings

../HeeRanjID/           ← sibling workspace — the HeeRanjId ID system
  heeranjid/            ← core Rust types and conversions
  heeranjid-sqlx/       ← PostgreSQL + SQLx integration
  heeranjid-ffi/        ← C FFI shared library
  bindings/             ← Python, TypeScript, .NET
```

Djogi calls into HeeRanjId for ID generation (`generate_id()` / `generate_ids(n)` / `generate_ranj_id()`) but does not own it. HeeRanjId is a standalone crate that Djogi depends on.

## Commands

```bash
# Build
cargo build

# Run tests
cargo test

# Run a single test
cargo test <test_name>

# Run tests for a specific crate
cargo test -p djogi-macros

# Check proc macro expansion (requires cargo-expand)
cargo expand -p djogi-macros

# Compile-fail tests (trybuild)
cargo test -p djogi-macros --test compile_fail

# Lint
cargo clippy --all-targets --all-features

# Format
cargo fmt --all

# CLI (once djogi-cli is implemented)
cargo djogi migrate
cargo djogi shell
cargo djogi db reset --seed
```

After implementation work, run `cargo fmt --all` and `cargo clippy --all-targets --all-features` before handoff when feasible, not just targeted tests.

## Architecture

### Crate Boundaries

| Crate | Role |
|---|---|
| `djogi` | Public API: `prelude`, `Model` trait, `QuerySet`, `ForeignKey`, `Jsonb<T>`, `ManyToMany`, app registration |
| `djogi-macros` | `#[derive(Model)]` proc macro — field injection, trait impls, `ModelDescriptor` emission via `inventory` |
| `djogi-cli` | `cargo djogi` subcommands via `clap` |
| `djogi-shell` | Rhai REPL, model bindings, transaction control |

### What Djogi Owns vs Delegates

Djogi is a Model-first framework — narrow in scope, deep within that scope. It targets **Postgres exclusively** (permanent design decision — JSONB, HeeRanjId, advisory locks, transactional DDL, and `RETURNING` all depend on it). It does **not** wrap or compete with:
- **Any web framework (Axum, Warp, Actix, Rocket, Poem, …)** — HTTP routing/middleware/extraction. Djogi's core is web-framework-agnostic; per-framework integrations (extractors that surface `DjogiContext`/`AuthContext` from request state, optional router-merging helpers) ship as opt-in sub-feature flags (`axum`, `warp`, `actix`, etc.). Adopters pick whichever HTTP layer fits their app and enable the matching flag — or none, if they wire integration manually.
- **SQLx** — Djogi wraps SQLx into a typed ORM layer (`Model`, `QuerySet`, `FromRow`, `ConditionBuilder`) but never hides it — raw `sqlx::QueryBuilder` is always an escape hatch.
- **HeeRanjId** — ID generation. Djogi calls `generate_id()` / `generate_ids(n)` / `generate_ranj_id()`.
- **Tokio** — async runtime. Used as-is.

### The `#[derive(Model)]` Macro

The macro is the heart of the framework. It:
1. Injects `id: HeerId`, `created_at: DateTime`, `updated_at: DateTime` as real struct fields
2. Implements the `Model` trait (CRUD methods)
3. Implements `FromRow` for SQLx deserialization
4. Generates `{Model}Fields` — typed field accessors for closure-based filter API
5. Generates `{Model}Filter` — programmatic filter builder for shell/dynamic use
6. Generates `{Model}Related` — prefetch selectors for FK relations
7. Emits `Model::descriptor()` via `inventory::submit!` for app registration and migration differ
8. Writes a side-channel `target/djogi_models.json` for `build.rs` consumption

Proc macro testing: use `trybuild` for compile-fail cases, `macrotest` for expansion snapshots.

### QuerySet and Condition Tree

`QuerySet<T>` is lazy — nothing hits the DB until a terminal method (`.fetch_all()`, `.fetch_one()`, etc.). It accumulates a typed `Condition` enum tree. The `ConditionBuilder` walks this tree and emits positional `$n` parameters via `sqlx::QueryBuilder<Postgres>`. Djogi owns this layer directly — no third-party query builder.

For queries beyond `QuerySet`, raw `sqlx::QueryBuilder` is always available as an escape hatch.

### Migration System

`build.rs` runs on every `cargo build`:
- Reads `target/djogi_models.json` (written by proc macros via `inventory`)
- Diffs against `migrations/schema_snapshot.json`
- Generates migration SQL pairs if drift detected; emits compiler warning (not error)

`migrations/` is a git submodule — managed by CI, not by the developer directly. `schema_snapshot.json` is updated only on successful `cargo djogi migrate`.

### Three-Database Architecture

At startup, Djogi maintains three connection pools:
- `url` — application data
- `crud_log_url` — structural CRUD audit (per-model `_logs` mirror tables in a separate DB)
- `event_log_url` — request/crash/debug events via `tracing`

Logging databases are isolated from the app DB so they survive `cargo djogi db reset`.

### Primary Keys — HeeRanjId

Two ID formats with a lossless upgrade path:

- **HeerId** (default): `BIGINT DEFAULT generate_id()` — 64-bit, time-ordered, populated via `RETURNING id`
- **RanjId** (opt-in): `UUID DEFAULT generate_ranj_id()` — 128-bit UUIDv8, sub-millisecond precision, higher node/sequence capacity. Opt in with `#[model(pk = "ranjid")]`
- **Serial** (opt-in): `#[model(pk = "serial")]` for lookup/reference tables

ID generation patterns:
- Default: DB generates via column default + `RETURNING id`
- Bulk: `HeerId::generate_many(&pool, n)` pre-allocates IDs before any INSERT
- Form pre-generation: ID generated at form render; INSERT uses `ON CONFLICT (id) DO NOTHING`
- JSON serialization: HeerId always serializes as `String` (JS loses precision on 64-bit integers); RanjId as standard UUID string

### `Jsonb<T>` Field Type

`Jsonb<T>` wraps a `JSONB` column with a typed schema. Internal layout:
```rust
pub struct Jsonb<T> {
    pub data: T,                           // typed, validated on save
    extra: IndexMap<String, UnknownField>, // unknown fields — never dropped
}
```
Unknown fields (present in DB, absent from schema) are preserved across every `save()`. All `UnknownField` conversions return `Result` — no implicit coercion. Validation runs the full `validator` tree before any DB write.

### Shell (Rhai)

The shell holds a dedicated single-threaded Tokio runtime. Every terminal method wraps its async implementation in `runtime.block_on(...)`. No `.await` in shell code — blocking is intentional. Shell error handling: one-liner printed, full traceback saved to `.djogi_shell_errors/`, session never unwound.

## Key Design Decisions (from spec)

- `create()` takes the struct directly — no separate `CreateVehicle` DTO
- No lazy loading — `.fetch()` and `.prefetch()` are always explicit
- All M2M relationships require explicit through models — implicit M2M fields are not provided
- M2M method names come from `const RELATION: &'static str` — no auto-pluralization
- FK cascade default is `RESTRICT` — must opt in to `cascade` per field
- Field renames: annotate with `#[field(renamed_from = "old_name")]` or the differ treats it as drop+add
- Admin panel is opt-in via `djogi = { features = ["admin"] }` — not bundled by default
- **Specialized features (admin, spatial, outbox publisher backends, vector, etc.) ship as feature flags within `djogi`, never as separate `djogi-*` crates.** The 4-crate workspace (djogi, djogi-macros, djogi-cli, djogi-shell) exists for hard Rust requirements (proc macro must be its own crate, CLI is a binary, shell is its own runtime) — it is not a template for fragmenting features. One `cargo add djogi`; pick capabilities via feature flags. The phrase "companion crate" in `docs/spec/` refers to user-side / app-side crates, not Djogi-maintained ones.
- `Djogi.toml` holds app config; secrets (DATABASE_URL, NODE_ID) live in env vars only

## Dependencies

Explicitly excluded (do not add):
- SeaORM / SeaQuery
- Diesel
- `chrono` (use `time` crate instead)
- Random UUID (v4) as default PK (use HeerId as default; RanjId for UUIDv8 when higher capacity needed)
- **`regex`, `regex-lite`, `fancy-regex`, `regex-automata`, or any other regex engine.** There shall never be a single line of regex in djogi — no regex-engine dependency, **and no regex notation in doc comments, commit messages, or any other in-repo text either.** Use byte-level checks (`u8::is_ascii_alphabetic`, `u8::is_ascii_alphanumeric`, explicit byte equality), sorted const slices with `binary_search`, and other stdlib primitives. Spell out rules in plain English ("ASCII letter or underscore followed by ASCII alphanumerics or underscores, up to 63 bytes"), not as bracket-class shorthand. See `docs/spec/decisions.md` for the formal rule.
