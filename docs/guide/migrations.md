# Migrations

Djogi's migration system is **descriptor-driven**: your model definitions are the desired-state source of truth, the differ produces SQL, and the runner applies it transactionally. You don't write `CREATE TABLE` by hand — you change a `#[model]` struct and let the system emit a reviewable migration pair.

```rust
#[model(table = "users", app = Auth)]
pub struct User {
    pub email: String,
    pub display_name: String,
    pub created_at: OffsetDateTime,
}
```

Add `pub bio: Option<String>` to the struct, run `cargo build`, and the build emits a drift warning. Run `djogi migrations compose --name add_user_bio` to generate `migrations/main/auth/V<timestamp>__add_user_bio.sql` (and `.down.sql`). Review the SQL in your PR. Apply via the public `djogi::migrate::apply_plan` library API. `attune` records, squashes, and publishes reviewed migration state; it does not execute migration SQL.

The system enforces three separate truths:

1. **Desired schema** — derived from `#[model]` descriptors at build time.
2. **Applied schema** — recorded in `migrations/<database>/<app>/schema_snapshot.json` after each successful apply.
3. **Operational history** — `.sql` files on disk + the `djogi_schema_migrations` ledger table.

Drift between any two surfaces is a typed diagnostic, not a silent recovery.

## The compose cycle

```
edit #[model]   →   cargo build (drift warning)   →   djogi migrations compose
                                                         ↓
                                              review V<ts>__name.sql + .down.sql
                                                         ↓
                                                   commit + open PR
                                                         ↓
                                          apply via `djogi::migrate::apply_plan`
                                                         ↓
                                  schema_snapshot.json updated atomically
```

`build.rs` is **diagnostic-only** — it never writes migration files, never mutates snapshots, and never touches the `migrations/` submodule. It reads `target/djogi_models.json` (proc-macro side-channel), compares against the committed snapshot + any pending JSON staging, and emits `cargo:warning=` lines on drift.

The compose step is operator-driven. Reviewers see the SQL diff in the PR; nobody approves a black box.

## Filesystem layout

```
migrations/                        ← git submodule
├── main/                          ← database name
│   ├── _global_/                  ← models with no #[model(app = ...)]
│   │   ├── V20260301000000__init.sql
│   │   ├── V20260301000000__init.down.sql
│   │   └── schema_snapshot.json
│   ├── auth/
│   │   ├── V20260315120000__add_user_bio.sql
│   │   ├── V20260315120000__add_user_bio.down.sql
│   │   └── schema_snapshot.json
│   └── billing/
│       └── ...
├── crud_log/                      ← separate database target
│   └── ...
└── event_log/
    └── ...
```

- One directory per `(database, app)` bucket. Cross-database FKs are rejected at projection time.
- Filename grammar: `V<14-digit timestamp>__<slug>.sql` (up) plus `.down.sql` (reverse).
- `_global_` is the synthetic bucket for models without an explicit `#[model(app = ...)]`.
- `schema_snapshot.json` is the per-bucket applied-state side-car. The runner persists it atomically after every transactional segment commits and the ledger row reaches `applied`.

The `migrations/` directory is a **git submodule**. It pins to a SHA in the parent repo so a checkout always matches the migration history that produced the schema. The `attune` command updates the parent's recorded pointer.

## The ledger

Every applied migration writes a row to `djogi_schema_migrations` (one ledger per database):

| Column | Purpose |
|---|---|
| `version` | The `V<ts>__<slug>` identifier from the filename |
| `app_label` | Bucket scoping — empty string for `_global_` |
| `applied_at_rank` | Monotonic ordinal — historical apply order |
| `checksum_up` / `checksum_down` | `V1:<sha256-hex>` content hash |
| `description` | Human-readable summary |
| `success` | `bool` — `false` means partial apply (see Repair) |
| `partial_apply_note` | Operator note when `success = false` or attune `--record` |
| `applied_at` | Wall-clock timestamp |

The `V1:<sha256-hex>` checksum prefix is intentional: future hash algorithms can coexist (`V2:...`) without ambiguity. Drift between disk SQL and the ledger checksum is detected at apply time and refuses without `repair`.

## CLI commands

### `djogi migrations compose`

```
djogi migrations compose [--name <slug>] [--allow-destructive] [--force-overwrite]
```

Composes a new migration from descriptor diff against the last committed snapshot.

- `--name <slug>` — operator-facing migration name. Sanitised to a strict identifier.
- `--allow-destructive` — required when the diff produces drops or touches a tombstoned app. Without this flag, destructive deltas refuse with a structural error.
- `--force-overwrite` — discards hand-edits to existing migration files (D013 override). The byte-equality check that gates this flag is purely structural — the deterministic emitter produces identical bytes on identical inputs, so any manual edit shows up as a difference.

Output: `V<timestamp>__<slug>.sql` + `.down.sql` per affected bucket, plus a per-bucket `target/djogi_pending/<database>/<app>.json` staging file.

### `djogi migrations status`

```
djogi migrations status
```

Read-only. Prints the current state of the ledger grouped by app, including:

- versions present in the ledger but absent from disk (and vice-versa)
- checksum-drift detection
- INVALID indexes (Postgres `pg_index.indisvalid = false` — typically from a failed `CONCURRENTLY` build)
- partial-apply rows

Status does NOT acquire the workspace lock, so it is safe to run while another operator is mid-apply.

### `djogi migrations attune`

```
djogi migrations attune [<git-target>]
                              [--apply]
                              [--record-ledger] [--record-reason "<note>"]
                              [--squash --from <version> [--app <label>]]
                              [--publish]
                              [--record]
```

Reconciles the local migration history with the ledger. Default mode is **read-only diff** — pass `--apply` to commit any mutation.

Three operational modes:

**Reconcile (default).** Scans on-disk SQL files vs ledger rows and prints discrepancies. With `<git-target>` (a local branch, tag, or remote ref), reconciles against that target; resolution tries local first, then falls back to `git fetch --all` + retry. With `--apply` AND a resolved `<git-target>` AND `--record`, also updates the parent repo's recorded submodule pointer to the target SHA.

**Record-ledger** (`--record-ledger`). Inserts ledger rows for SQL files that exist on disk but have no ledger entry. Records the operator-supplied `--record-reason` in `partial_apply_note`. Does NOT execute the SQL — assumes the migrations were applied out-of-band (typical recovery flow after a manual `psql` apply).

**Squash** (`--squash --from <version>`). Coalesces every committed migration from `<version>` onward into a single squashed migration. **History rewrite** — gated on localhost + dev profile + `dev_mode = true` + `DJOGI_ENV=dev`. Use `--app <label>` to disambiguate when `<version>` exists in multiple buckets. `--publish` pushes the rewritten submodule to its remote; without it, the rewrite stays local.

Exit codes: `0` on success, `1` on runtime error (config / network / SQL / git), `2` on refusal (gate failure or arg validation).

## Library APIs

The CLI dispatchers for `apply` / `rollback` / `fake` / `baseline` / `repair` / `verify` are deferred to a Phase 7 follow-up. Until then, library callers use the public entry points directly:

```rust
use djogi::migrate::{
    apply_plan, rollback_plan, fake_apply_plan, baseline_plan,
    verify, RunnerCtx, WorkspaceGuard,
};
use djogi::migrate::repair::{
    repair_checksum_drift, repair_partial_apply,
    repair_resume_partial_apply, repair_snapshot_rebuild,
};
```

| API | What it does |
|---|---|
| `apply_plan(ctx, plan, runner_ctx, guard)` | Acquires advisory lock, inserts pending ledger row, dispatches segments transactionally / non-transactionally per `Classification`, persists snapshot, marks ledger `applied`. |
| `rollback_plan(ctx, plan, runner_ctx, guard, lossy_policy, prior_snapshot)` | Replays the down-side SQL in reverse segment order, marks ledger row removed, and applies the caller-selected `LossyRollbackPolicy` for ops that cannot be cleanly reversed. |
| `fake_apply_plan(ctx, plan, runner_ctx, guard, reason)` | Inserts the ledger row WITHOUT executing SQL — for migrations applied out-of-band (e.g. via a hot-fix `psql` script). Equivalent to `attune --record-ledger` for one version, with the operator reason persisted in the ledger note. |
| `baseline_plan(ctx, bucket, runner_ctx, guard, reason)` | Marks an existing schema as the baseline for a migration bucket — inserts ledger rows for committed migrations without executing them. Used when adopting Djogi against a pre-existing database. |
| `verify(ctx, snapshot)` | Compares live `pg_catalog` shape against the snapshot. Returns a `VerifyReport` with per-diagnostic severity. |
| `repair_*` | Four typed repair flows: checksum drift, partial-apply cleanup, resume after interrupted apply, snapshot rebuild from ledger. |

All apply paths require a `WorkspaceGuard` — a typed witness that the caller holds the workspace file lock. The lock prevents two concurrent CLI invocations from racing on the same `migrations/` tree.

## Classifications

Every `SchemaDelta` carries a `Classification` that determines runner behaviour:

| Classification | Meaning | Runner behaviour |
|---|---|---|
| `NoOp` | Schemas compared equal | No segments emitted |
| `Additive` | Only `AddTable` / `AddColumn` (nullable or default-having) / `AddIndex` / `AddEnum` / `AddEnumVariant` / `AddForeignKey` | Applied without gating |
| `Reversible` | At least one rename, no drops | Applied without gating; clean inverse |
| `Destructive` | Contains drops (`DropTable`, `DropColumn`, `DropIndex`, `DropForeignKey`, `DropEnum`) | Refused unless `--allow-destructive` |
| `Lossy` | Drops a non-nullable, non-default column (data loss with no fallback) | Stricter than `Destructive` — runner refuses regardless of flag; rollback paths route through `LossyRollbackPolicy::Allow { reason }` to opt in |
| `Unsupported { reason }` | Differ cannot lower the change safely | Operator must hand-edit the migration |
| `PkTypeFlip { co_destructive, co_lossy }` | At least one PK-type flip | Routes through T9's expand/contract orchestration; `co_*` flags surface co-existing severity so destructive gating still applies even when the headline classification is the flip |

Phase 7.5 layered a second classification dimension (`OnlineSafe` / `FastLockDestructiveGuarded` / `ExpandContract` / `OfflineOnly`) that captures lock-time and live-row impact orthogonally. The two dimensions compose: a change can be `Reversible` on the structural axis and `ExpandContract` on the online-safety axis (typical of FK additions), and the runner gates on both.

## PK-type flip migrations

Phase 7 T9 ships native support for PK-type flips between the four built-in HeerId / RanjId variants (`HeerId`, `HeerIdRecencyBiased`, `RanjId`, `RanjIdRecencyBiased`). The differ detects the flip, the segment planner emits the expand/contract dance (shadow column → backfill → cutover → finalise), and the runner walks segments transactionally where safe and non-transactionally where required.

```rust
// Before
#[model(table = "events", pk = HeerId)]
pub struct Event { /* ... */ }

// After — flip to recency-biased
#[model(table = "events", pk = HeerIdRecencyBiased)]
pub struct Event { /* ... */ }
```

`djogi migrations compose` produces a single migration containing the full DAG: drop dependent FKs, prepare shadow column, backfill, swap columns, recreate FKs in dependency order. Composite cycles, partitioned tables, and join tables are handled. PK flips that involve a **custom** PK (declared via `djogi::primary_key!`) — either a Custom-to-Custom shape change or a Custom↔built-in transition — are explicitly rejected at compose time in v0.1.0 with a typed `SchemaOperation::Unsupported` diagnostic that surfaces the `type_name`, `sql_type`, and `default_sql` of both sides; adopters who genuinely need such a flip must write the migration by hand. See `docs/spec/migrations.md` §10.10a (Primary-Key Flip Support Matrix) for the full reject rationale and the post-v0.1.0 extensibility plan.

## Out-of-order policy

Operators sometimes apply migrations in a different order than they were composed (typical scenario: feature branch A composed migration `V100`, branch B composed `V101`, B merged first). The runner's policy field controls how this is handled:

| Policy | Behaviour |
|---|---|
| `Reject` | Refuse with a structural error. Default on `production` profile and CI. |
| `AllowWithDiagnostic` | Apply but log a diagnostic. Default on `dev` profile. |
| `Allow` | Silent. Manual override only. |

The policy lives in `RunnerCtx::out_of_order_policy` and defaults from `Djogi.toml::profile`.

## `djogi db reset`

```
djogi db reset --yes [--allow-checksum-drift-reset] [--maintenance-database <name>]
```

Drops, recreates, and replays every committed migration against the application database. **Triple-gated**:

1. `DATABASE_URL` resolves to a localhost connection (`127.0.0.1` / `::1` / `localhost`).
2. `Djogi.toml::profile != "production"`.
3. `--yes` is passed (or the operator confirms an interactive y/N prompt).

Only the application database is touched. Logging databases (`crud_log`, `event_log`) survive — they hold cross-incident audit trails that should outlast schema iteration.

`--maintenance-database <name>` selects the admin DB used for the `DROP DATABASE` / `CREATE DATABASE` round-trip. Defaults to `postgres`. Override for clusters that use a different admin DB (e.g. AWS RDS uses `rdsadmin`).

URL paths are percent-decoded and validated against the strict Postgres-identifier grammar before splicing into DDL — defence-in-depth against URL-injection.

Before `DROP DATABASE`, reset compares the live ledger's recorded migration checksums against the current committed migration files. Edited `up.sql`, edited `down.sql` (when the ledger carries a down checksum), missing historical files, or historical baseline rows whose checksums cannot be compared to file bytes all refuse with exit code `2` before any destructive step. `--allow-checksum-drift-reset` is the explicit operator override for that refusal path.

Exit codes: `0` success, `1` runtime error, `2` gate refusal.

## `djogi db seed`

```
djogi db seed [--database <name>] [--allow-non-localhost]
```

Runs operator-authored SQL seed files in `seeds/<database>/*.sql` alphabetically. Idempotent — re-runs skip seeds whose `V1:<sha256>` checksum matches the `djogi_seed_runs` ledger; refuses on checksum drift.

`--database <name>` selects BOTH the seed directory and the connection target. The CLI splices `<name>` into the application URL's path component so seeds always land on the matching DB; a malformed application URL refuses with exit code `1`.

`--allow-non-localhost` opens the gate for CI integration suites seeding remote test databases. The gate is lighter than `db reset`'s — seeds are intentionally additive and idempotent.

Exit codes: `0` success, `1` runtime error (config / network / SQL / checksum drift / malformed URL), `2` gate refusal (non-localhost without `--allow-non-localhost`).

## `djogi docs`

```
djogi docs [--output <path>]
```

Renders Markdown reference pages from the descriptor inventory — one file per registered model under `<output>/<app>/`, plus a top-level `README.md` index. Output defaults to `target/djogi-docs/` and is byte-deterministic against the same descriptor set.

Each page covers the table name, every field's name + Rust type + SQL type + nullable + default, declared indexes, and FK targets. The `Default` column is populated from the PK strategy via the projection mirror, so descriptor-emitted defaults (for example `heerid_next_desc()` for the default `HeerIdRecencyBiased` PK or `heerid_next()` for explicit `pk = HeerId`) appear on every model that uses them.

## Test-time helpers

`#[djogi_test(sync_models = [...])]` materialises a model set into the per-test ephemeral database without going through the full migration pipeline:

```rust
#[djogi_test(sync_models = [User, Post, Tag])]
async fn user_can_create_post(mut ctx: DjogiContext) {
    let u = User::create(&mut ctx, ...).await.unwrap();
    let p = Post::create(&mut ctx, ...).await.unwrap();
    /* ... */
}
```

`sync_models` reuses Phase 7's `project_from_iters → diff_bucket_maps → plan_delta` pipeline (the same code paths the production runner uses), then executes the additive plan directly without ledger writes, advisory locks, or classification gating. The Phase 7 test suite includes a parity test (`tests/integration/phase7_t10_sync_models_parity.rs`) that runs both `sync_models` and `apply_plan` against fresh databases and asserts byte-identical `pg_catalog` shape — drift between the two execution wrappers is caught before merge.

The pre-flight FK validator refuses if any model in the supplied set references a target that isn't also in the set (FK-target-missing error names both the source column and the missing target).

For multi-database tests, `setup_test_db()` / `teardown_test_db()` are also exported as `pub` from `djogi::testing` for hand-written harnesses that need more than one ephemeral DB.

## Escape hatches

Three intentional escape hatches sit alongside the descriptor-driven flow:

1. **Author-edited `.sql`.** The compose flow generates SQL files; nothing prevents an operator from editing them before commit. The byte-equality check at compose time catches accidental edits when re-composing the same delta. For deliberate edits (e.g. backfill UPDATE inside an additive migration), pass `--force-overwrite` on the next compose. The deterministic emitter still re-emits the structural baseline, so an author-supplied `UPDATE` is preserved only if it lives in a separate file or in the migration body alongside the descriptor-generated DDL.

2. **`fake_apply_plan` / `attune --record-ledger`.** Insert ledger rows for migrations applied out-of-band. Used after a manual `psql` recovery: the SQL has already run, the ledger needs to catch up.

3. **`baseline_plan`.** Mark an existing schema as the baseline when adopting Djogi against a pre-existing database. Inserts ledger rows for every committed migration through a target version without executing them.

Repair flows (`repair_checksum_drift`, `repair_partial_apply`, `repair_resume_partial_apply`, `repair_snapshot_rebuild`) cover the four scenarios where state has drifted between disk / ledger / snapshot and need explicit operator intervention to converge.

## Exit code matrix

Every `db` / `migrations` subcommand follows a uniform exit-code convention:

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | Runtime error — config, network, SQL, replay, malformed URL |
| `2` | Refusal — policy gate (not localhost, production profile, missing `--yes`, etc.) OR clap-style argument validation |

`1` and `2` bundle slightly differently than typical CLIs: argument-validation errors land at `2` rather than `1` so CI scripts can treat any `2` as a soft "operator must intervene" skip without disambiguating gate-vs-arg.

## Further reading

- [Apps](./apps.md) — `(database, app)` bucketing, retirement flow with tombstones.
- [Migrations spec](../spec/migrations.md) — full design rationale, T1–T10 task scope, decision log.
