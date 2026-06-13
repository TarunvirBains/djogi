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

Add `pub bio: Option<String>` to the struct, run `cargo build`, and the build emits a drift warning. Run `djogi migrations compose --name add_user_bio` to generate `migrations/main/auth/V<timestamp>__add_user_bio.sdjql` (and `.down.sdjql`). Review the SQL in your PR. Apply via the public `djogi::migrate::apply_plan` library API. `attune` records, squashes, and publishes reviewed migration state; it does not execute migration SQL.

The system enforces three separate truths:

1. **Desired schema** — derived from `#[model]` descriptors at build time.
2. **Applied schema** — recorded in `migrations/<database>/<app>/schema_snapshot.json` after each successful apply.
3. **Operational history** — `.sdjql` files on disk + the `djogi_schema_migrations` ledger table.

Drift between any two surfaces is a typed diagnostic, not a silent recovery.

## The compose cycle

```
edit #[model]   →   cargo build (drift warning)   →   djogi migrations compose
                                                         ↓
                                              review V<ts>__name.sdjql + .down.sdjql
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
│   │   ├── V20260301000000__init.sdjql
│   │   ├── V20260301000000__init.down.sdjql
│   │   └── schema_snapshot.json
│   ├── auth/
│   │   ├── V20260315120000__add_user_bio.sdjql
│   │   ├── V20260315120000__add_user_bio.down.sdjql
│   │   └── schema_snapshot.json
│   └── billing/
│       └── ...
├── crud_log/                      ← separate database target
│   └── ...
└── event_log/
    └── ...
```

- One directory per `(database, app)` bucket. Cross-database FKs are rejected at projection time. Cross-app FKs **within the same database** are fully supported — when a compose produces multiple buckets at the same version and one references another via foreign key, Djogi automatically derives the dependency graph and orders the apply so referenced tables are created first.
- Sharing a `DjogiEnum` across apps works without special configuration: compose deduplicates shared enum types automatically, emitting exactly one `CREATE TYPE` in the first bucket (by alphabetical order when no FK edges exist). The other buckets that reference the same enum get a dependency edge pointing to the owning bucket, so they apply after the type exists. You just derive `DjogiEnum` once and use it from models in any app — compose and apply handle the rest.
- **Upgrading from global to per-bucket enum snapshots.** If your `schema_snapshot.json` files were recorded by an older Djogi version that stored enum entries globally (before per-bucket scoping was introduced), running `djogi migrations compose` once will silently advance each stale snapshot to the current scoped state and print a `snapshot converged: <database>/<app>` line per affected bucket. No migration file is generated; this is a one-time convergence step that brings the on-disk snapshots in sync with the current layout so subsequent builds no longer emit a spurious "run compose" warning.
- Filename grammar: `V<14-digit timestamp>__<slug>.sdjql` (up) plus `.down.sdjql` (reverse).
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
| `checksum_up` / `checksum_down` | `V1:<sha256-hex>` hash of canonical operation SQL fragments |
| `description` | Human-readable summary |
| `success` | `bool` — `false` means partial apply (see Repair) |
| `partial_apply_note` | Operator note when `success = false` or attune `--record` |
| `applied_at` | Wall-clock timestamp |

The `V1:<sha256-hex>` checksum prefix is intentional: future hash algorithms can coexist (`V2:...`) without ambiguity. For Djogi-composed migrations, headers and label comments are outside the checksum domain; `checksum_down = NULL` means there is no real rollback SQL beyond comment placeholders. Operation SQL drift between disk and the ledger is detected at apply/reset time and refuses without `repair` or an explicit reset drift override.

When a migration is applied, the "already applied" check is scoped per `(version, app_label)` — the same version string can exist in multiple buckets independently. A `compose` run stamps one version across all affected buckets, and each bucket's migration applies and is tracked in its own ledger stream.

## CLI commands

### `djogi migrations compose`

```
djogi migrations compose [--name <slug>] [--allow-destructive] [--force-overwrite]
```

Composes a new migration from descriptor diff against the last committed snapshot.

- `--name <slug>` — operator-facing migration name. Sanitised to a strict identifier.
- `--allow-destructive` — required when the diff produces drops or touches a tombstoned app. Without this flag, destructive deltas refuse with a structural error.
- `--force-overwrite` — discards hand-edits to existing migration files (D013 override). The byte-equality check that gates this flag is purely structural — the deterministic emitter produces identical bytes on identical inputs, so any manual edit shows up as a difference.

Output: `V<timestamp>__<slug>.sdjql` + `.down.sdjql` per affected bucket, plus per-bucket pending staging under `target/djogi_pending/<database>/<app>.json`. Auto-emitted Phase 0 uses the hidden namespace `target/djogi_pending/<database>/.phase_zero/<version>.json` so it can coexist with normal global pending and diagnostics can name the hidden artifact distinctly from `_global_.json`. New compose output never writes Phase 0 to `_global_.json`; legacy normal-global Phase 0 pending is read only by the bootstrap emitter as a compatibility fallback when the hidden file is absent.

### `djogi migrations apply`

```
djogi migrations apply [--fake --reason "<text>"]
```

Applies pending migrations discovered from `target/djogi_pending/`.

- `djogi migrations apply` applies SQL and snapshots exactly as composed.
- `djogi migrations apply --fake --reason "<text>"` records the migration as
  `faked` in the ledger without executing SQL; `--reason` is required and
  must be non-empty.
- When there are no pending migrations, the command is a no-op and exits `0`
  (no identity or pool validation).
- Exit codes: `0` success, `1` transient runtime error (connection, pool,
  SQL execution failure — CI may retry), `2` refusal — the condition is
  deterministic and requires operator action: a missing or invalid node
  identity, a checksum mismatch or format error on a committed migration,
  a version collision, a stale Phase 0 artifact, a segment execution-mode
  conflict, a relpages threshold refusal, a target-table-not-found, an
  out-of-order rejection under `Reject` policy, a PK-flip pre-flight
  hazard, a snapshot persist failure after the ledger row was applied, or
  an advisory-unlock correctness failure.

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

The `apply` command ships as `djogi migrations apply` (with `--fake` / `--reason` flags for existing-database adoption). The `verify` command ships as `djogi migrations verify`. The `repair` family ships as `djogi migrations repair <checksum-drift|partial-apply|resume-partial|snapshot-rebuild>` (see **Repair Commands** below). The `baseline` command ships as `djogi migrations baseline` (see **Baseline Command** below). The `rollback` CLI dispatcher is deferred; library callers use the public entry point directly:

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
| `apply_plan(ctx, plan, runner_ctx, guard)` | Acquires advisory lock, inserts pending ledger row, dispatches segments transactionally / non-transactionally per `Classification`, marks the ledger row `applied`, then persists the snapshot. Snapshot persistence failure is post-`applied`: the row remains applied and the operator repairs the snapshot separately. |
| `rollback_plan(ctx, plan, runner_ctx, guard, lossy_policy, prior_snapshot)` | Replays the down-side SQL in reverse segment order, marks the ledger row `rolled_back`, and applies the caller-selected `LossyRollbackPolicy` for ops that cannot be cleanly reversed. |
| `fake_apply_plan(ctx, plan, runner_ctx, guard, reason)` | Inserts the ledger row WITHOUT executing SQL — for migrations applied out-of-band (e.g. via a hot-fix `psql` script). Equivalent to `attune --record-ledger` for one version, with the operator reason persisted in the ledger note. |
| `baseline_plan(ctx, bucket, runner_ctx, guard, reason)` | Projects the bucket's live database catalog into a single `baseline` ledger row (no SQL runs against user tables; `checksum_up` is content-addressed over the projection) and persists that projection as the bucket's canonical snapshot. Used when adopting Djogi against a pre-existing database whose schema already exists. Refuses a caller-supplied `runner_ctx.snapshot` (it always projects fresh, so a stale snapshot cannot poison future diffs). Exposed on the CLI as `djogi migrations baseline`. |
| `verify(ctx, snapshot)` | Compares live `pg_catalog` shape against the snapshot. Returns a `VerifyReport` with per-diagnostic severity. |
| `repair_*` | Four typed repair flows: checksum drift, partial-apply cleanup, resume after interrupted apply, snapshot rebuild from ledger. |

All apply paths require a `WorkspaceGuard` — a typed witness that the caller holds the workspace file lock. The lock prevents two concurrent CLI invocations from racing on the same `migrations/` tree.

**Out-of-order policy enforcement:** `fake_apply_plan` enforces the same out-of-order policy gate as `apply_plan`. A faked row with a suppressed `out_of_order_flag` would misrepresent the version-ordering state. If the policy is `Reject`, fake-apply on an out-of-order version is rejected.

**Snapshot failure recovery:** If `fake_apply_plan` reports a snapshot persistence error, the migration was successfully recorded in the ledger as `faked`. Run `djogi migrations compose` to regenerate the snapshot from the descriptor inventory.

## Node Identity for Migration Commands

Djogi migration commands that execute user SQL or generate run IDs require an explicit node identity. There are four separate identity boundaries:

1. **Runtime application pools** — caller-owned via `post_connect`. The Djogi runtime library and pool constructors (`DjogiPool::connect`, `from_database_config`) do NOT read `HEER_NODE_ID` automatically. Wire node GUCs explicitly in your `post_connect` hook.
2. **Migration CLI resolver** — identity-bearing CLI commands (`migrations apply`, `migrations baseline`, `db reset`, `repair resume-partial`) support `--node-id <id>` and `--single-node-dev` flags. The resolver selects explicit `--node-id` over `HEER_NODE_ID` env var. Values outside `0..=511` refuse with exit code 2 before database work.
3. **Migration runner library** — the runner binds the selected node on the pinned migration session before non-Phase-0 `generate_run_id` / `HeeRanjID` calls and before user SQL execution. Missing identity is refused before session pinning or ledger mutation.
4. **Phase 0 bootstrap** — production/cluster Phase 0 installs HeeRanjID schema/functions without node seed or database-level defaults. The canonical Phase 0 SQL remains identity-free; explicit `--single-node-dev` provisions node 1 after Phase 0 SQL succeeds and before the ledger row is marked applied.

### CLI Identity Flags

```bash
# Selected-node mode (requires pre-registered active node in heer_nodes)
djogi migrations apply --node-id 7

# Single-node development mode (provisions node 1, uses database-level fallbacks)
djogi migrations apply --single-node-dev

# Environment fallback (explicit --node-id wins over HEER_NODE_ID)
HEER_NODE_ID=3 djogi migrations apply

# Refused: conflicting flags
djogi migrations apply --node-id 7 --single-node-dev  # error

# Refused: missing identity for non-dev mode (exit code 2)
djogi migrations apply  # error — requires --node-id, HEER_NODE_ID, or --single-node-dev

# Refused: single-node-dev in production (exit code 2)
DJOGI_ENV=production djogi migrations apply --single-node-dev  # error
```

### Identity-Free Paths

These paths do NOT require node identity because they neither execute user migration SQL nor call non-Phase-0 run ID generation: `migrations status`, `migrations verify`, all `migrations attune` modes, `repair checksum-drift`, `repair partial-apply` status updates, and `repair snapshot-rebuild`. When no pending migrations exist, `migrations apply` prints a no-op message without identity resolution.

### Phase 0 Bootstrap Modes

Production/cluster Phase 0 installs HeeRanjID schema, functions, and required extensions only — no node seed, no database-level GUC defaults.

Explicit `--single-node-dev` keeps the on-disk Phase 0 SQL identity-free, then the runner provisions node 1 with dynamic `current_database()` database defaults after Phase 0 SQL succeeds and before `mark_applied`. If that provisioning fails, the ledger row is marked `failed` and the snapshot is not written. `db reset --single-node-dev` inherits the same replay behavior, leaving node 1 usable after reset.

### Stale Phase 0 Protection

Phase 0 artifact preflight is scoped to paths that would replay or record Phase 0 SQL. `apply`, `fake apply`, `repair resume`, reset replay, and CLI reapply cleanup allow only identity-free replay-current Phase 0 artifacts before mutation; seed-capable runtime helper SQL and seed-DML non-runtime artifacts are refused for replay. `rollback` preflights the authoritative materialized down SQL before changing ledger status or running down-side SQL, so seed-capable, seed-DML non-runtime, ambiguous/comment-only, and generated-stale Phase 0 down payloads refuse early. `migrations attune` remains identity-free; only Record/Squash with `--apply` refuse seed-capable, seed-DML non-runtime, ambiguous, or generated-stale Phase 0 files before ledger/file mutation. `baseline` does not broaden into Phase 0 artifact preflight; it keeps its snapshot refusal and identity checks.

The Phase 0 classifier recognizes only exact banner lines for the current and legacy production/seeded banners. Identity-free production artifacts are the only replay-current shape; seeded current artifacts are runtime-only. Banner text embedded in SQL literals, suffixed banner strings, mixed literal/dynamic database defaults, seed-free incomplete generation, generated-stale literal database defaults, and non-runtime top-level seed-table mutation against HeeRanjID seed tables all fail closed on replay paths. The seed mutation scanner covers direct `INSERT`/`UPDATE`/`DELETE`, CTE-led data mutations, `MERGE INTO`, and `COPY ... FROM`, while skipping comments, strings, quoted identifiers, and dollar-quoted bodies. Rollback is stricter still: Phase 0 rollback requires a non-empty down payload that classifies as identity-free replay-current before any transactional or non-transactional down SQL or ledger mutation begins.

## Repair Commands

`djogi migrations repair <subcommand>` exposes the four operator-confirmed
repair flows from the CLI. Invoking the subcommand IS the operator
acknowledgment — there is no separate `--confirm` flag. Each command pins one
Postgres session, takes the per-bucket advisory lock, and holds the workspace
file lock for its duration. Exit codes: `0` success, `1` runtime/I/O error
(retryable), `2` refusal or structural mismatch (operator must intervene).

```bash
# Re-checksum a ledger row after its committed SQL was edited. Omitting
# --checksum-up / --checksum-down recomputes them from the committed files
# (a missing down file is a no-op).
djogi migrations repair checksum-drift V20260101000000__add_users \
  --checksum-up V1:<hex> --checksum-down V1:<hex>

# Resolve a partial-apply row by rewriting its status. Does NOT execute SQL.
djogi migrations repair partial-apply V20260101000000__add_users rolled-back \
  --note "reverted by hot-fix psql script"

# Resume an interrupted non-transactional apply by replaying remaining steps
# from the committed <version>.plan.json.
djogi migrations repair resume-partial V20260101000000__add_index_concurrently

# Rebuild a bucket's schema snapshot from the ledger + live database.
djogi migrations repair snapshot-rebuild --app billing
```

All four accept `--app` (bucket app label; empty for the global bucket),
`--database` (defaults to `main`), and `--workspace` (workspace-root override).

## Baseline Command

`djogi migrations baseline <version> --reason "<why>"` adopts an existing
database under Djogi's migration ledger. Use it when the schema already exists
(from a prior tool, manual DDL, or a restored backup) and `compose` + `apply`
cannot run against the populated database without a starting point.

```bash
# Establish a baseline for the global bucket of the main database.
djogi migrations baseline V00000000000000__baseline \
  --reason "schema pre-exists from prior tooling"

# Baseline a specific app bucket in a non-default database, with a
# custom ledger description.
djogi migrations baseline V00000000000000__baseline \
  --reason "imported from legacy system" \
  --description "legacy billing schema" \
  --app billing --database crud_log
```

What it does: projects the bucket's **live** Postgres catalog into a single
ledger row with `status = 'baseline'`, computes `checksum_up` as a
content-addressed hash of that projection, and persists the projection as the
bucket's canonical `schema_snapshot.json`. **No SQL runs against user tables** —
the schema is captured exactly as Postgres currently holds it, and future
`compose` / `verify` runs diff against that captured snapshot. The command pins
one Postgres session, takes the per-bucket advisory lock, and holds the
workspace file lock for its duration. Invoking the command IS the operator
acknowledgment.

| Argument / flag | Description |
|---|---|
| `<version>` | Version label for the baseline ledger row (e.g. `V00000000000000__baseline`). Must be unique in the ledger. |
| `--reason TEXT` | **Required, non-empty.** Recorded in the ledger row's audit note (`partial_apply_note`). An empty reason is refused (exit 2). |
| `--description TEXT` | One-line ledger description. Defaults to `existing database schema baseline`. |
| `--app LABEL` | Bucket app label. Defaults to the global bucket (empty string). |
| `--database NAME` | Database name. Defaults to `main`. `crud_log` / `event_log` route to their configured per-database URLs. |
| `--workspace PATH` | Workspace-root override. Defaults to the current working directory. |

Exit codes: `0` success, `1` runtime error (config / pool / SQL failure), `2`
refusal — empty `--reason`, an unresolvable database URL, a duplicate version
that already carries a ledger row, a snapshot persist failure after the ledger
row was applied, a session-pinning correctness failure (`pg_advisory_unlock`
returned false), or a Postgres server below version 18.

> **One baseline per bucket.** A bucket should carry at most one `baseline` row.
> Re-running `baseline` with a version that already exists in the ledger refuses
> (exit 2); choose a fresh version string if you genuinely need to re-baseline.

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

## RolledBack Recovery

After a migration is rolled back, the ledger row has status `rolled_back`. This is a non-terminal status — the DDL effects are gone from the database, but the audit trail remains.

To re-apply a rolled-back migration, run `djogi migrations apply` again. The CLI apply path:
1. Detects the existing `rolled_back` row for the version
2. Deletes the reapply-blocking row as cleanup before invoking the runner
3. Invokes the runner to apply the migration SQL and create a new `applied` ledger row

This preserves the original version-to-schema-operation binding without requiring the operator to invent a new version string.

## `djogi db reset`

```
djogi db reset --yes [--single-node-dev] [--allow-checksum-drift-reset] [--maintenance-database <name>]
```

Drops, recreates, and replays every committed migration against the application database. **Requires explicit `--single-node-dev`** — selected-node reset (`--node-id` or `HEER_NODE_ID`) refuses before destructive operations because drop/create removes the old `heer_nodes` registration.

**Triple-gated**:

1. `DATABASE_URL` resolves to a localhost connection (`127.0.0.1` / `::1` / `localhost`).
2. `Djogi.toml::profile != "production"`.
3. `--yes` is passed (or the operator confirms an interactive y/N prompt).

Only the application database is touched. Logging databases (`crud_log`, `event_log`) survive — they hold cross-incident audit trails that should outlast schema iteration.

`--maintenance-database <name>` selects the admin DB used for the `DROP DATABASE` / `CREATE DATABASE` round-trip. Defaults to `postgres`. Override for clusters that use a different admin DB (e.g. AWS RDS uses `rdsadmin`).

URL paths are percent-decoded and validated against the strict Postgres-identifier grammar before splicing into DDL — defence-in-depth against URL-injection.

Before `DROP DATABASE`, reset compares the live ledger's recorded migration checksums against the current committed migration files. Edited up-side `.sdjql`, edited down-side `.down.sdjql` (when the ledger carries a down checksum), missing historical files, or historical baseline rows whose checksums cannot be compared to file bytes all refuse with exit code `2` before any destructive step. `--allow-checksum-drift-reset` is the explicit operator override for that refusal path.

Exit codes: `0` success, `1` runtime error, `2` gate refusal.

## `djogi db seed`

```
djogi db seed [--database <name>] [--allow-non-localhost]
```

Runs operator-authored SQL seed files in `seeds/<database>/*.sql` alphabetically. The runner pins one session, takes a per-database advisory lock, records a claim-first `djogi_seed_runs` row before each seed body executes, and then finalises that row to `applied` or `failed`. Re-runs skip seeds whose `V1:<sha256>` checksum already matches an `applied` row, refuse on checksum drift, and also refuse on stale `running` claims or prior `failed` claims so non-idempotent seed SQL is never silently replayed.

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

1. **Author-edited `.sdjql`.** The compose flow generates SQL files; nothing prevents an operator from editing them before commit. The byte-equality check at compose time catches accidental edits when re-composing the same delta. For deliberate edits (e.g. backfill UPDATE inside an additive migration), pass `--force-overwrite` on the next compose. The deterministic emitter still re-emits the structural baseline, so an author-supplied `UPDATE` is preserved only if it lives in a separate file or in the migration body alongside the descriptor-generated DDL.

2. **`fake_apply_plan` / `attune --record-ledger`.** Insert ledger rows for migrations applied out-of-band. Used after a manual `psql` recovery: the SQL has already run, the ledger needs to catch up.

3. **`djogi migrations baseline` / `baseline_plan`.** Project an existing database's live schema into a single `baseline` ledger row + canonical snapshot when adopting Djogi against a pre-existing database. No SQL runs against user tables — the live catalog is captured as-is and becomes the starting point future migrations diff against.

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
