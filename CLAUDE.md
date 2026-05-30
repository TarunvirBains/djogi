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
  djogi-cli/            ← djogi binary
  djogi-shell/          ← Rhai engine + model bindings (Phase 9 target)
  djogi-maahi/          ← planned admin console crate (Dioxus full-stack); not present as shipped component in this worktree

../HeeRanjID/           ← sibling workspace — the HeeRanjId ID system
  heeranjid/            ← core Rust types and conversions
  heeranjid-sqlx/       ← PostgreSQL + SQLx integration
  heeranjid-ffi/        ← C FFI shared library
  bindings/             ← Python, TypeScript, .NET
```

Djogi calls into HeeRanjId for ID generation (`heerid_next()` / `heerid_next_desc()` / `ranjid_next()` / `ranjid_next_desc()`, plus `generate_ids(...)` / `generate_ranjids(...)` batch helpers) but does not own it. HeeRanjId is a standalone crate that Djogi depends on.

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

# Macro fixture gate — lihaaf (currently 325 fixtures: 320 default + 4 spatial + 1 network)
cargo lihaaf --manifest-path djogi-macros/Cargo.toml -j 4

# Raw-SQL bypass fixture gate — lihaaf (currently 42 fixtures: 39 default + 3 spatial)
cargo lihaaf --manifest-path djogi/Cargo.toml -j 4

# Re-bless lihaaf compile_fail snapshots after diagnostic changes
cargo lihaaf --manifest-path djogi-macros/Cargo.toml --filter compile_fail --bless -j 4
cargo lihaaf --manifest-path djogi/Cargo.toml --bless -j 4

# Lint
cargo clippy --all-targets --all-features

# Format
cargo fmt --all

# Secret-pattern preflight (URLs with creds, named secret env vars, PEM
# private keys). Runs in CI; also use --staged before commit and --stdin
# when drafting public issues / PR bodies. See docs/guide/secrets-hygiene.md.
cargo xtask check-secrets
cargo xtask check-secrets --staged
cargo xtask check-secrets --stdin < draft.md

# CLI (Phase 7 + later phases)
djogi migrations apply               # apply pending migrations (canonical spelling)
djogi migrations apply --fake \      # fake-apply for existing-DB adoption
  --reason "schema pre-exists"
djogi migrate apply                  # alias for djogi migrations apply
djogi migrations compose             # generate up/down SQL pair from descriptor drift
djogi migrations status              # show ledger / snapshot / live-DB state
djogi migrations attune              # reconcile disk / ledger / live DB
djogi db reset --yes                 # drop, recreate, replay (triple-gated)
djogi db seed                        # run seeds/<database>/*.sql via djogi_seed_runs ledger
djogi docs                           # render Markdown reference pages from descriptor inventory
# djogi shell                          # Rhai shell (Phase 9 target; deferred in v0.1.0)
```

After implementation work, run `cargo fmt --all` and `cargo clippy --all-targets --all-features` before handoff when feasible, not just targeted tests.


## PR Hygiene

Every PR body **must** include a closing keyword + issue reference for each issue it resolves. GitHub auto-closes issues on merge only when one of these keywords is present in the PR description — not from commit messages, branch names, or titles. This applies to ALL PRs across all projects.

Valid closing keywords: `close`, `closes`, `closed`, `fix`, `fixes`, `fixed`, `resolve`, `resolves`, `resolved`.

Convention: use `Closes` consistently across all PRs for uniformity.

Format:
```
Closes #356
Closes #357
```

## Worktree workflow

When running concurrent careful-coder dispatches across multiple `.worktrees/`
checkouts, parallel cargo builds previously corrupted each other's shared
target tree (djogi#176). Per-worktree target isolation avoids incremental-build
interference. Give each worktree its own physically separate target tree by
overriding `CARGO_TARGET_DIR`.

Two ways to enable per-worktree isolation:

```bash
# direnv (recommended): per-worktree opt-in, no shell-init pollution
cp .envrc.example .envrc && direnv allow

# manual (no direnv): source the example, or inline the same SHA-256 id
# (using basename "$PWD" would collide between sibling worktrees and
# orphan caches when `cargo xtask gc-target-cache` runs).
source .envrc.example
# — or, self-contained one-shot —
export CARGO_TARGET_DIR="$HOME/.cache/djogi-target/$(printf '%s' "$(pwd -P)" | sha256sum | cut -c1-12)"
```

`.envrc.example` derives a stable 12-char id from the absolute worktree path
so siblings sharing a basename do not collide. CI runners are single-worktree
per job and do not need this fix; the override is a developer-side convenience
only.

Tradeoff: each worktree's cache accumulates roughly 5-10 GB of incremental
artifacts. After `git worktree remove`, prune orphaned caches with
`cargo xtask gc-target-cache`.

## Architecture

### Crate Boundaries

| Crate | Role |
|---|---|
| `djogi` | Public API: `prelude`, `Model` trait, `QuerySet`, `ForeignKey`, `Jsonb<T>`, `ManyToMany`, app registration |
| `djogi-macros` | `#[derive(Model)]` proc macro — field injection, trait impls, `ModelDescriptor` emission via `inventory` |
| `djogi-cli` | Standalone `djogi` binary and subcommands via `clap` |
| `djogi-shell` | Rhai REPL, model bindings, transaction control |

### What Djogi Owns vs Delegates

Djogi is a Model-first framework — narrow in scope, deep within that scope. It targets **Postgres 18 and later, exclusively** (permanent design decisions — JSONB, HeeRanjId, advisory locks, transactional DDL, `RETURNING`, and latest Postgres features all depend on it; earlier versions explicitly unsupported per `docs/spec/decisions.md`). It does **not** wrap or compete with:
- **Any web framework (Axum, Warp, Actix, Rocket, Poem, …)** — HTTP routing/middleware/extraction. Djogi's core is web-framework-agnostic; per-framework integrations (extractors that surface `DjogiContext`/`AuthContext` from request state, optional router-merging helpers) ship as opt-in sub-feature flags (`axum`, `warp`, `actix`, etc.). Adopters pick whichever HTTP layer fits their app and enable the matching flag — or none, if they wire integration manually.
- **`tokio-postgres` + `deadpool-postgres`** — Djogi wraps these into a typed ORM layer (`Model`, `QuerySet`, `FromPgRow`, `ConditionBuilder`). Raw SQL remains available as a deliberate escape hatch, gated by the raw SQL bypass harness described in [`docs/spec/raw-sql-escape-hatches.md`](docs/spec/raw-sql-escape-hatches.md).
- **HeeRanjId** — ID generation. Djogi calls `heerid_next()` / `heerid_next_desc()` / `ranjid_next()` / `ranjid_next_desc()`, plus `generate_ids(...)` / `generate_ranjids(...)` batch helpers.
- **Tokio** — async runtime. Used as-is.

### The `#[derive(Model)]` Macro

The macro is the heart of the framework. It:
1. Injects `id: HeerId`, `created_at: DateTime`, `updated_at: DateTime` as real struct fields
2. Implements the `Model` trait (CRUD methods)
3. Implements `FromPgRow` for `tokio-postgres` row deserialization
4. Generates `{Model}Fields` — typed field accessors for closure-based filter API
5. Generates `{Model}Filter` — programmatic filter builder for shell/dynamic use
6. Generates `{Model}Related` — prefetch selectors for FK relations
7. Emits `Model::descriptor()` via `inventory::submit!` for app registration and migration differ
8. Writes a side-channel `target/djogi_models.json` for `build.rs` consumption

Proc macro testing: use `lihaaf` for compile-fail/compile-pass cases (the
sole compile-fixture gate, fast parallel dylib path); `macrotest` for
expansion snapshots. Djogi no longer uses `trybuild` — the historical
trybuild corpus was migrated to lihaaf in Phase 8.5, with the `.stderr`
snapshots committed under `djogi-macros/tests/compile_fail/` and
`djogi/tests/compile_fail/`.

### QuerySet and Condition Tree

`QuerySet<T>` is lazy — nothing hits the DB until a terminal method (`.fetch_all()`, `.fetch_one()`, etc.). It accumulates a typed `Condition` enum tree. The `ConditionBuilder` walks this tree and emits positional `$n` parameters via `pg::accumulator::SqlAccumulator`, which collects raw SQL fragments + bound values for `tokio_postgres::Client::query`. Djogi owns this layer directly — no third-party query builder.

## Raw SQL is djogi's `unsafe`

Raw SQL in djogi is treated culturally the way `unsafe` is in Rust: not
banned, but always conscious. The mechanism enforces this at compile
time; the convention enforces it in code review.

**The mechanism.** The raw SQL escape hatches (`raw_execute`,
`raw_query`, `raw_rows`, `raw_fetch_one`, `raw_scalar`, `raw_ddl`,
`raw_stream`, `raw_stream_with_fetch_size`) live on the
`djogi::__bypass::RawAccessExt` trait and are unreachable from
`DjogiContext` without the bypass attribute. `pool()`, `conn()`,
`with_client`, and `batch_execute` are similarly gated. Direct use of
`tokio_postgres::Client` or `deadpool_postgres::Pool` is gated by a
workspace `clippy::disallowed_methods` lint.

**The bypass attribute.** To use any raw escape - typically in a
dedicated pin test under `tests/pin/`, or in a deliberately
unidiomatic helper - decorate the enclosing item:

    #[djogi::deliberately_bypass_convention_with_raw_sql]
    // JUSTIFICATION (djogi#234): citext column needs case-insensitive
    // equality; QuerySet doesn't expose LOWER(col) equality yet.
    async fn my_test(mut ctx: DjogiContext) { ... }

**The `// JUSTIFICATION (djogi#<n>):` convention.** Every use of the
attribute under `tests/` MUST be paired with a `JUSTIFICATION` comment
syntactically attached to the decorated item, validated by
`cargo xtask check-justifications`. The
issue number references **djogi's** tracker (`djogi#<n>` is GitHub
cross-repo notation), not your application's - reaching for raw_*
signals a gap in djogi's typed surface, and that gap belongs to djogi
to fix.

**Pin tests** under `tests/pin/` use `JUSTIFICATION (PIN): exercises
raw_<api> itself` instead of an issue number. Pin tests are the
legitimate carve-out - one per raw API.

**Ordinary tests.** Every other integration test under
`tests/integration/` must exercise the typed surface: `Model::create`,
`Model::save`, `Model::delete`, `Model::objects()`,
`djogi::transaction::atomic`, and `#[djogi::djogi_test(sync_models = [...])]`.
This repository's tests may not manually reference `djogi::__bypass`; use
the bypass attribute so the use site stays auditable.

**No ergonomic raw SQL.** djogi will not ship a fluent `ctx.raw().execute(...)`
shortcut or a `RawSqlBuilder`. Every reach for raw SQL walks through the
verbose attribute and the justification. Friction is the design.

The harness has no runtime grep gate; the type system, clippy, and the
xtask validator are the enforcement. See [`docs/spec/raw-sql-escape-hatches.md`](docs/spec/raw-sql-escape-hatches.md).

### Migration System

`build.rs` runs on every `cargo build`:
- Reads `target/djogi_models.json` (written by proc macros via `inventory`)
- Diffs against `migrations/schema_snapshot.json`
- Generates migration SQL pairs if drift detected; emits compiler warning (not error)

`migrations/` is a git submodule — managed by CI, not by the developer directly. `schema_snapshot.json` is updated only on successful runs of `djogi migrations apply` (or the library entry point `djogi::migrate::apply_plan`). The runner persists the snapshot atomically after every transactional segment commits and the ledger row reaches `applied`.

### Three-Database Architecture

At startup, Djogi maintains three connection pools:
- `url` — application data
- `crud_log_url` — structural CRUD audit (per-model `_logs` mirror tables in a separate DB)
- `event_log_url` — request/crash/debug events via `tracing`

Logging databases are isolated from the app DB so they survive `djogi db reset`.

### Primary Keys — HeeRanjId

Two ID formats with a lossless upgrade path:

- **HeerIdRecencyBiased** (default): `BIGINT DEFAULT heerid_next_desc()` — 64-bit, newest-first sort order, populated via `RETURNING id`
- **HeerId** (opt-in): `BIGINT DEFAULT heerid_next()` — 64-bit, ascending / time-ordered. Opt in with `#[model(pk = HeerId)]`
- **RanjId** (opt-in): `UUID DEFAULT ranjid_next()` — 128-bit UUIDv8, sub-millisecond precision, higher node/sequence capacity. Opt in with `#[model(pk = RanjId)]`
- **Serial** (opt-in): `#[model(pk = Serial)]` for lookup/reference tables

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
- **Specialized features (spatial, outbox publisher backends, vector, etc.) ship as feature flags within `djogi`, never as separate `djogi-*` crates.** The workspace includes crates for hard Rust boundaries (library, macros, CLI, shell runtime) and keeps admin as a planned carve-out: **djogi-maahi** is not in shipped components in this branch and remains a Phase 10 dependency target. The "one `cargo add djogi`" experience is preserved conceptually, but `features = ["admin"]` is not yet available until Maahi ships. The phrase "companion crate" in `docs/spec/` refers to user-side / app-side crates, not Djogi-maintained ones.
- `Djogi.toml` holds app config; secrets (DATABASE_URL, HEER_NODE_ID) live in env vars only

## Tests must use djogi structs, not raw escape hatches

**Integration tests (under `tests/integration/`) MUST NOT call `DjogiContext::raw_execute`, `raw_query`, `raw_scalar`, or `raw_ddl`.** The lone exception per API is one dedicated pin test that exercises that API's own behaviour.

Every fixture constructs database state through djogi's typed surface:

- `#[djogi::djogi_test(sync_models = [Model, ...])]` for table creation (the macro calls `djogi::testing::sync_models` for you, which projects the descriptor through `pk_default_sql` and dispatches DDL — the projection layer stays in the call chain)
- `Model::create` / `Model::save` / `Model::delete` for row writes
- `Model::objects()` and the queryset for reads

Why: every `raw_*` method accepts a SQL string the test composed by hand. That string never traverses the projection layer, so projection bugs — wrong function names, missing identifier-length checks, defaults that don't exist on the target Postgres — never surface from the test surface. `raw_ddl` carries the same blast radius as `raw_execute` (it is `batch_execute(sql)` under a friendlier name); the layering benefit only accrues when `sync_models` is in the call chain. Tracking issue: GH #133.

## Dependencies

Explicitly excluded (do not add):
- SeaORM / SeaQuery
- Diesel
- `chrono` (use `time` crate instead)
- Random UUID (v4) as default PK (use HeerId as default; RanjId for UUIDv8 when higher capacity needed)
- **`regex`, `regex-lite`, `fancy-regex`, `regex-automata`, or any other regex engine.** There shall never be a single line of Rust regex in djogi — no regex-engine dependency, **and no regex notation in doc comments, commit messages, or any other in-repo text describing framework-internal rules.** Use byte-level checks (`u8::is_ascii_alphabetic`, `u8::is_ascii_alphanumeric`, explicit byte equality), sorted const slices with `binary_search`, and other stdlib primitives. Spell out rules in plain English ("ASCII letter or underscore followed by ASCII alphanumerics or underscores, up to 63 bytes"), not as bracket-class shorthand. **Carve-out:** Postgres POSIX regex matching is a SQL feature (`~` / `~*` operators), exposed as `FieldRef::regex` / `iregex` and `Lookup::Regex`. That surface is permitted because the match runs server-side and no Rust regex code is linked — Djogi exposes a Postgres feature, not a Rust regex API. See `docs/spec/decisions.md` for the formal rule.
