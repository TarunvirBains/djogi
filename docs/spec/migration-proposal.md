> [Back to README](../../README.md) | [All Specs](./index.md)

# Migration System — Team Review Proposal

> **Historical — superseded 2026-04-23.** This proposal fed into the Phase 7 v3 and Phase 7-Zero v3 synthesis. Examples in this document (notably the `djogi::apps!` bare-label syntax, the single-level `migrations/<app>/schema_snapshot.json` path shape, the old `schema drift detected` warning text, and legacy `_up.sql` / `_down.sql` filename examples) predate the current rulings. For authoritative syntax and paths, see `docs/spec/decisions.md` (rows dated 2026-04-23), `docs/spec/apps-and-database-domains.md`, and `docs/spec/migrations.md`. Kept as a design-history record; do not implement against examples here.

**Date:** 2026-04-22
**Status:** Proposal for team review — historical
**Supersedes on acceptance:** `docs/spec/migrations.md` §10, two rows in `docs/spec/decisions.md`
  ("Build drift diagnostic" and "Migration generation")
**Prior art:** `docs/research/migrations/2026-04-22/` — 16,198 lines of source-backed research
  across 11 migration systems, 12 topic syntheses, 1 gap analysis, 1 locked-recommendations doc,
  1 decision record

---

## Executive Summary

This proposal packages the locked migration-system design from
`docs/research/migrations/2026-04-22/16-decision-record.md` as a reviewable document positioned
alongside the existing `docs/spec/migrations.md`, `docs/spec/decisions.md`, and the Phase 7
architecture/plan pair. It does not rewrite the existing docs in-place. On team acceptance, the
supersession table in Part IV drives the actual doc updates. Until then, existing docs stand.

The research that underlies this proposal was systematic. Eleven production migration systems were
studied at source-code depth: Flyway, Liquibase, Prisma, Alembic, Django, SQLAlchemy, Diesel,
SeaORM, sea-query, refinery, and cot. Twelve topic syntheses covered ledger schema, checksums,
advisory locks, transactional semantics, out-of-order policy, baseline adoption, rename handling,
composite indexes, destructive classification, online-safe DDL, diff algorithms, and the Rust
ecosystem contrast specifically. The gap analysis (`13-gap-analysis-vs-current-spec.md`) found
29 validated decisions, 21 gaps, and 3 live contradictions in the existing docs. The
recommendations document (`14-locked-recommendations.md`) resolved every gap and contradiction
with a concrete, cited decision. The decision record (`16-decision-record.md`) locks them all.
Total primary-source material: approximately 16,200 lines. Total project notes: approximately
5,500 lines across 11 system write-ups.

The headline change from the existing plan: `build.rs` no longer writes migration SQL files.
It emits a plain cargo warning on drift; the developer explicitly runs `djogi migrations
compose` to generate migration files. This is the only item that contradicts currently-locked
decisions in `docs/spec/decisions.md`. Every other change is additive (new commands, richer
ledger, apps subsystem) or clarifying (precise advisory lock key, finalized ledger DDL, checksum
format).

The team is being asked to evaluate ten design pillars and their downstream consequences, with
special attention to:

1. `build.rs` is diagnostic-only (re-opens two locked decisions — the only true re-open in
   the whole proposal).
2. The apps subsystem (`djogi::apps!` macro, per-app migration folders, lifecycle operations
   for rename/remove/move) — entirely new, not in the Phase 7 plan, emerged from the walkthrough.
3. The finalized ledger DDL — five columns beyond the Phase 7 v2 plan's draft DDL.
4. The three-file snapshot model — `target/` vs `migrations/` separation with atomic writes.
5. The complete CLI surface under the `djogi migrations *` noun-grouped verb convention.

---

## How to Read This Document

This proposal is designed to be read alongside `docs/spec/migrations.md` and the tracked
Phase 7 migration research bundle in `docs/research/migrations/2026-04-22/`. Each section in Parts I
and II is structured as: current plan summary, proposed design, rationale, tradeoffs. Team
members already familiar with the Phase 7 docs can skim the "Current plan" sub-sections and
focus on "Proposed" and "Rationale." Part III (Lifecycle Walkthrough) is the most persuasive
section for any reviewer uncertain about the practical impact — read it before forming an
opinion on the architecture sections.

---

## Part I: The Ten Design Pillars

These are the load-bearing choices. Each downstream detail in Parts II and III follows from one
or more of these. If a pillar is re-opened, the sections that depend on it are noted.

### Pillar 1: Descriptor-first, with a side-car snapshot (not shadow DB, not embedded)

Djogi plans migrations by diffing model descriptors against `migrations/schema_snapshot.json`.
It never diffs against the live database catalog at plan time, and it never embeds snapshot
structs inside migration files. The snapshot is an explicit, committed side-car: reviewable,
versionable, and independent of the live DB.

Rationale: the stored-snapshot approach is O(1), deterministic, and requires no live DB
connection at diff time. Shadow DB (Prisma's approach) is the most accurate but requires
`CREATE DATABASE` permission and a full schema replay per diff — unacceptable as an every-build
workflow. Embedded snapshots (cot's approach) couple execution plans to model shapes and hit
`todo!()` on field-type changes at
`cot-cli/src/migration_generator.rs:835` (T12, T11).

Re-opening this pillar means re-doing the source-of-truth and diff-algorithm topic research
sweeps (T01, T11).

### Pillar 2: Djogi-owned runner on tokio-postgres (not sqlx::migrate)

The migration runner is entirely Djogi-owned: planner, SQL emitter, advisory lock, ledger
writes, and snapshot updates all live in `djogi/src/migrate/`. No `sqlx::migrate` API surface
survives.

Rationale: `sqlx::migrate`'s minimalist two-column ledger cannot support the status column,
`run_id`, `app_label`, or `applied_steps_count` columns required by this design. `sqlx::migrate`
also does not use advisory locking, meaning a concurrent apply can corrupt the ledger silently
(T12, T04). This decision was already locked in the Phase 7 design and v2 plan; it is listed
here because `docs/spec/migrations.md` §10.1 still says "Execution is sqlx's built-in runner"
and must be updated.

### Pillar 3: Per-migration advisory lock with Djogi-specific key

Every `djogi migrations apply` invocation acquires `pg_try_advisory_lock(4994068948568834898)`
before reading the pending set. The key is the ASCII bytes of `DJOGMIGR` packed into a 64-bit
integer (`0x444A4F474D494752`), derived and documented — not a magic constant. Release is via
`pg_advisory_unlock(...)` in a finally-equivalent path; if the process is killed, Postgres
releases the lock when the TCP connection tears down.

Rationale: the key is distinct from Prisma's hardcoded `72707369` (which differs by approximately
5 trillion) and outside Flyway's derived key range (T04, R-03). No Rust migration system in the
survey uses advisory locking; this is a genuine Djogi differentiator and a correctness requirement
for any multi-process deploy scenario.

### Pillar 4: SHA-256 checksum with V1: format prefix (Liquibase-style versioning)

Checksum format: `V1:` followed by 64 lowercase hex characters (total 67 chars, stored in
`VARCHAR(68)`). Input: raw UTF-8 SQL bytes after BOM-strip and line-ending normalization to `\n`.
What is NOT hashed: filename, version number, description — only SQL content.

Rationale: SHA-256 (Prisma's algorithm) has a 256-bit collision space versus Flyway's CRC-32
(32-bit, stored as signed integer) and refinery's SipHash-1-3 (non-cryptographic, and refinery
hashes name+version+sql, meaning a file rename changes the checksum). The `V1:` prefix follows
Liquibase's `V:hex` versioned format — a future algorithm upgrade increments to `V2:`, and
existing `V1:` rows remain valid (T03, R-05).

### Pillar 5: Two-bucket destructive classifier (Prisma pattern, escalated)

Operations that generate SQL are classified into two buckets before file generation:

- `unexecutableSteps` — hard-block generation; requires `djogi migrations compose
  --allow-destructive`. Includes: `DROP TABLE`, `DROP COLUMN`, nullable-to-NOT NULL without
  DEFAULT, enum value deletion, enum value reorder.
- `warnings` — proceed with generation but emit `-- DJOGI WARNING:` comment in the UP file.
  Includes: type narrowing, `DROP INDEX`, `DROP FOREIGN KEY`, annotated renames.

Djogi escalates `DROP TABLE` and `DROP COLUMN` from Prisma's `warnings` bucket to
`unexecutableSteps`. The research position: irreversible data loss requires explicit opt-in,
not a log message (T09, R-18).

### Pillar 6: Explicit CLI-only file generation (build.rs is read-only)

`build.rs` emits one plain cargo warning on schema drift. It does not write any files. Migration
SQL files are generated exclusively by explicit `djogi migrations compose` invocation.

This is the only pillar that contradicts currently-locked decisions. It re-opens the "Build drift
diagnostic" and "Migration generation" rows in `docs/spec/decisions.md`. The re-open rationale:
`build.rs` writing to `migrations/` (a git submodule) on every `cargo build` causes IDE churn
(directory watchers, LSP re-indexing) and bypasses the developer review step that is the primary
safety gate for migration SQL. The Phase 7 design doc (`P7D §Core Model`) already states "`build.rs`
may read the snapshot. It must never mutate it." Diagnostic-only extends that principle
consistently (T12, R-12).

### Pillar 7: Opt-in apps with compile-time sealed enum

A `djogi::apps!` macro defines a sealed enum of app labels per crate. Models opt in via
`#[model(app = AppName)]`. Cross-app FK dependencies are resolved from the type graph at
compile time — no runtime lookups, no manual `dependencies = [...]` lists. Users who never
invoke `djogi::apps!` retain the current flat layout unchanged.

Rationale: this is the biggest additive piece of the proposal. It emerged from the app-label
question during the OI-01 walkthrough and became a full design. It enables per-app migration
folders, per-app snapshots, and app-level lifecycle operations (rename, remove, move-model).
The compile-time sealed trait ensures only declared apps appear in the `app_label` column.
There is no close analog in the surveyed systems (Django's app concept is the inspiration, but
Django's FK dependency graph is resolved at runtime). See §2.5 for full detail.

Re-opening this pillar means removing the apps subsystem entirely. It does not affect Pillars
1–6 or 8–10.

### Pillar 8: Atomic single-point snapshot writes

The `schema_snapshot.json` file is written exactly once per successful `djogi migrations
apply` run: after the final statement of the final migration commits, via atomic
`tmp → fsync → rename`. The snapshot never represents partial state. Non-transactional migrations
track progress via `applied_steps_count` in the ledger, not via intermediate snapshot writes.

Rationale: a snapshot that represents partial state undermines the core invariant that the
snapshot equals what the DB has. Per-statement snapshot writes create transient states that
cannot be distinguished from crash states (T01, OI-06).

### Pillar 9: Four-variant lifecycle markers (rename, tombstone, move, renamed_from)

Schema objects that undergo lifecycle events declare them via source-level attributes:

- `#[field(renamed_from = "old_name")]` — field rename (existing decision, unchanged)
- `#[model(renamed_from = "old_table")]` — table rename (R-16)
- `#[app(renamed_from = "old_label")]` — app rename (new)
- `#[app(tombstone)]` — app retirement (new)
- `#[model(moved_from_app = "old_label")]` — model moved between apps (new)

All markers are migration-window-only: the differ detects stale annotations (annotation present
in source, snapshot already reflects the change) and emits a hard error requiring cleanup (R-20).

### Pillar 10: First-class repair, baseline, verify, and status commands

the repair, baseline, verify, and status migration flows are core engine
deliverables, not appendix material. All four are registered in the CLI today
as `djogi migrations status`, `djogi migrations repair`,
`djogi migrations baseline`, and `djogi migrations verify`, each backed by a
public library entry point.
The research finding that motivated this: every system that has been run at production scale
either built first-class repair/adoption tooling or accumulated painful war stories about
operators hand-editing the ledger table (T03, C-08 in doc 15).

---

## Part II: Architecture

### 2.1 The CLI Surface — `djogi migrations *`

The existing Phase 7 v2 plan uses Django-inspired command names: `makemigrations`, `migrate`.
This proposal replaces them with noun-grouped verbs throughout. The shipped CLI surface is narrower than the full target design:

| Command | What it does |
|---|---|
| `djogi migrations compose` | Generate migration SQL files from model descriptors (was `makemigrations`) |
| `djogi migrations compose --allow-destructive` | Allow `unexecutableSteps` operations |
| `djogi migrations compose --name <slug>` | Override auto-generated migration description |
| `djogi migrations status` | Show pending and applied migration state |
| `djogi migrations attune` | Reconcile migration history state through the shipped attune workflow |
| Shipped target verb | `apply` ships as `djogi migrations apply` |
| Target verbs | `apply` ships as `djogi migrations apply`; `verify` as `djogi migrations verify`; `repair` as `djogi migrations repair`; `baseline` as `djogi migrations baseline`; and `rollback` as `djogi migrations rollback` |
| `djogi migrations help [<subcommand>]` | Print help for the group or a specific subcommand |
| `djogi migrations` (no subcommand) | Equivalent to `help` — prints subcommand list + common workflows |

Every reference to `makemigrations` in existing Phase 7 docs and in the existing spec is
superseded by `migrations compose`. Every bare `migrate` is superseded by `migrations apply`.
The noun-grouped form makes the command surface self-documenting: `djogi migrations <tab>`
reveals the full surface; no knowledge of which verbs are standalone vs. subcommands is needed.

**Current plan (Phase 7 v2):** `djogi makemigrations`, `djogi migrate`, `djogi
migrate show`, `djogi migrate repair`, `djogi migrate baseline`, `djogi plan`
(the existing plan uses the cargo-subcommand prefix throughout).

**Change:** The noun-grouped convention replaces all of the above. `djogi plan` is retired;
its output is absorbed into `migrations status` with structured `HistoryDiagnostic` taxonomy
(R-25).

**Binary and invocation forms.** The shipped `djogi-cli` package declares the standalone
`djogi` binary. Canonical form throughout this proposal is `djogi migrations ...`; any
cargo-subcommand wrapper is a future packaging decision, not a current install surface.

**Help and discoverability.** Every subcommand supports `--help` / `-h`. Running `djogi
migrations` with no subcommand prints help (does not error). `djogi migrations help
<subcommand>` and `djogi migrations <subcommand> --help` are equivalent. The bare-group
help output includes a "Common Workflows" section listing the most frequent command sequences
(first-time setup, post-edit dev cycle, catching up on teammate changes, diagnosing problems).
Subcommand-specific help includes a `DESCRIPTION`, `EXAMPLES`, `SEE ALSO`, and `DIAGNOSTICS`
section — the latter references the D001–D025 codes relevant to that subcommand. Help output
is coloured via `owo-colors` / `termcolor` and respects the `NO_COLOR` env var. Help works
without a database connection and without the migrations submodule being initialised, so new
users can discover the surface before committing to any state changes. This same help pattern
applies to every noun group Djogi's CLI gains (`db`, `shell`, `admin`, `models`, etc.) — the
`migrations` group sets the template.

### 2.2 The Runner

**Current plan (`docs/spec/migrations.md` §10.1):** "Execution is sqlx's built-in runner —
checksummed, tracked in `_sqlx_migrations`."

**Proposed:** Djogi-owned runner on `tokio-postgres 0.7` + `deadpool-postgres 0.14`. The runner
acquires a dedicated single `tokio-postgres` connection (not a pool connection) for the migration
apply window. Pool connections cannot hold advisory locks safely: if the connection is recycled,
the lock persists until pool teardown (R-23, T04).

**Key differences from Phase 7 v2 plan draft:** The v2 plan already states "No `sqlx::migrate`
compatibility layer survives" (Critical Design Decision 1). This proposal makes it explicit
that the runner uses a dedicated non-pooled connection for the advisory lock lifecycle, which
the v2 plan does not specify.

**Tradeoff:** Implementing the runner from scratch is non-trivial. The payoff is that every
ledger column in §2.3, every diagnostic in §2.6, and every partial-apply recovery path in §2.8
becomes achievable. `sqlx::migrate` cannot be extended to support them.

### 2.3 The Ledger

The ledger table is named `djogi_schema_migrations`. The finalized DDL (supersedes the Phase 7
v2 plan draft, which lacked five columns):

```sql
CREATE TABLE IF NOT EXISTS djogi_schema_migrations (
    -- Surrogate PK for stable row identity and temporal ordering
    id                    BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,

    -- Natural version key: one canonical ledger row per migration version
    version               TEXT          NOT NULL UNIQUE,
    description           TEXT          NOT NULL DEFAULT '',

    -- Checksums: SHA-256 hex with V1: prefix (Pillar 4)
    checksum_up           VARCHAR(68)   NOT NULL,
    checksum_down         VARCHAR(68),   -- NULL only when no _down.sql paired file exists

    -- Execution mode
    execution_mode        TEXT          NOT NULL DEFAULT 'transactional'
                              CHECK (execution_mode IN ('transactional', 'non_transactional')),

    -- Lifecycle status (Prisma pre-write row pattern).
    -- Out-of-order is tracked in `out_of_order_flag` below, NOT in this
    -- enum — status describes lifecycle, the flag is orthogonal.
    status                TEXT          NOT NULL DEFAULT 'pending'
                              CHECK (status IN (
                                  'pending', 'applied', 'baseline',
                                  'faked', 'rolled_back', 'failed'
                              )),

    -- Timestamps
    applied_at            TIMESTAMPTZ   NOT NULL DEFAULT now(),
    applied_by            TEXT          NOT NULL DEFAULT current_user,
    execution_time_ms     BIGINT        NOT NULL DEFAULT 0,

    -- Out-of-order flag
    out_of_order_flag     BOOLEAN       NOT NULL DEFAULT false,

    -- Partial-apply state (non-transactional migrations)
    applied_steps_count   INTEGER       NOT NULL DEFAULT 0,
    total_steps           INTEGER,       -- NULL for transactional; statement count otherwise
    partial_apply_note    TEXT,          -- operator note written during repair

    -- Deployment group: HeerId from SELECT heerid_next() at run start
    run_id                BIGINT        NOT NULL,

    -- App label (empty string = global/flat layout)
    app_label             TEXT          NOT NULL DEFAULT '',

    -- Snapshot version this migration was applied against
    snapshot_version      TEXT          NOT NULL
);

CREATE INDEX djogi_schema_migrations_status_idx
    ON djogi_schema_migrations (version)
    WHERE status != 'applied';

CREATE INDEX djogi_schema_migrations_run_id_idx
    ON djogi_schema_migrations (run_id);
```

**Column-by-column differences from Phase 7 v2 plan draft:**

| Column | v2 plan draft | This proposal | Source |
|---|---|---|---|
| `id` | Missing (version was PK) | `BIGINT IDENTITY PRIMARY KEY` | OI-05 — stable row identity + temporal ordering without giving up `version UNIQUE` |
| `status` | Not present | 6-variant CHECK enum (out-of-order tracked separately via `out_of_order_flag`) | R-06 — pre-write row eliminates crash window |
| `applied_by` | Not present | `DEFAULT current_user` | R-04 — Flyway `installed_by` pattern |
| `run_id` | `TEXT` nullable | `BIGINT NOT NULL` (HeerId) | OI-01 — HeerId is the canonical Djogi ID type |
| `app_label` | Not present | `TEXT NOT NULL DEFAULT ''` | Part IV of decision record — apps subsystem |
| `applied_steps_count` | `partial_apply_state TEXT` (free-text) | `INTEGER NOT NULL DEFAULT 0` + `total_steps INTEGER` | OI-05 — queryable, repair-integrable |
| `execution_time_ms` | Not present | `BIGINT NOT NULL DEFAULT 0` | R-04 — Flyway `execution_time` pattern |

**The `status` column semantics:**
- `pending` — written before DDL executes; the crash-detection marker.
- `applied` — DDL succeeded and committed.
- `baseline` — row inserted by `migrations baseline` without running DDL.
- `faked` — row inserted by `migrations apply --fake` without running DDL.
- `rolled_back` — `_down.sql` executed successfully.
- `failed` — DDL failed; `applied_steps_count` captures how far a non-transactional migration got.

Out-of-order migrations keep whichever of the six lifecycle statuses actually applies (normally `applied`) and additionally set `out_of_order_flag = true`. Status describes lifecycle; the flag is orthogonal. This matches the canonical DDL in `docs/spec/migrations.md`.

**Important Djogi choice vs. Prisma:** Prisma's ledger keeps failed rows as audit trail and may
insert a fresh row for the same migration name during repair flows. Djogi does **not** adopt that
multi-row-per-migration pattern in the main ledger. `version` remains unique, and a single
canonical row per migration version is updated through its lifecycle (`pending -> applied`,
`pending -> failed`, `failed -> rolled_back`, `failed -> applied`, etc.). Prisma is the prior art
for `applied_steps_count` and explicit lifecycle-state modeling, not for duplicate-version rows in
Djogi's canonical ledger.

The `applied_at` column stores the moment the row was written, for all row types including
`baseline` and `faked`. The `status` column carries semantic meaning; `applied_at` answers
"when was this row written," not "when did the migration first run in production." (OI-03)

**The `run_id` column:** One call to `SELECT heerid_next()` at the start of each
`djogi migrations apply` invocation produces a HeerId stamped into every ledger row
written during that run. This is Liquibase's `DEPLOYMENT_ID` concept adapted to HeerId — no
other Rust migration system provides deployment-level grouping. Post-mortems that ask "what
changed in last Tuesday's deploy?" become `WHERE run_id = ...` queries. (OI-01)

### 2.4 The Snapshot Model

Three files, three roles:

| File | Location | Role | Committed to git? |
|---|---|---|---|
| `target/djogi_models.json` | build artifact | what Rust source currently declares | no (gitignored) |
| `target/djogi_pending/<app>.json` | build artifact | generated by `compose`, pre-apply target state | no (gitignored) |
| `migrations/<app>/schema_snapshot.json` | submodule | what is currently applied in the DB | yes |

**`build.rs` three-way match logic:**

1. `djogi_models.json` equals `schema_snapshot.json` for all apps → silent.
2. Mismatch detected, AND `target/djogi_pending/<app>.json` matches `djogi_models.json` for the
   drifted app → `cargo:warning=djogi: migration pending — apply via djogi::migrate::apply_plan`.
3. Mismatch detected, AND no matching pending file → `cargo:warning=djogi: schema drift detected
   — run \`djogi migrations compose\``.

**Lifecycle:** `migrations compose` writes `target/djogi_pending/<app>.json` and generates the
SQL pair. The library apply path consumes the pending file (atomic `tmp → fsync → rename` into the
submodule snapshot, then deletes the pending file) on successful completion; this ships as
`djogi migrations apply`. (OI-04)

**Crash recovery:** If the process is killed between ledger COMMIT and snapshot rename, the DB
is fully applied but the snapshot is stale. `djogi migrations verify` (or
`djogi::migrate::verify`) detects the discrepancy, and `djogi migrations repair
snapshot-rebuild` (or the `djogi::migrate::repair_*` helpers) regenerates from
the current ledger plus source descriptors. (OI-06)

**Snapshot format:** The `schema_snapshot.json` file includes a top-level `format_version: 1`
field. The runner rejects snapshots with an unknown `format_version`. After a branch merge that
produces a merge conflict in the snapshot file, the resolution is: fix any migration file
conflicts, then run `djogi migrations compose` to rebuild the conflict-free snapshot (R-26).

### 2.5 Apps Subsystem (New — Team Scrutiny Requested)

This subsystem was not in the Phase 7 plan. It emerged from the `run_id`/app-label discussion
during the research walkthrough and became a full design. It is the biggest additive piece of
this proposal and the section the team should scrutinize most carefully before acceptance.

**What it is:** A compile-time sealed enum of app labels, defined once per crate. Apps are
organizational metadata — they group migrations into per-app folders and tag ledger rows.

**The `djogi::apps!` macro:**

```rust
// src/apps/mod.rs
djogi::apps! {
    Vehicles,
    Users,
    Orders,
}
```

This expands to one ZST struct per variant (`pub struct Vehicles;`), one `impl djogi::App for
Vehicles` with `const LABEL: &'static str = "vehicles"` (auto-lowercased, overridable via
`#[app(label = "...")]`), and a sealed-trait implementation that prevents freeform
`impl djogi::App for Foo` from outside the macro. A second `djogi::apps!` invocation in the
same crate is a compile-time error.

**Opting a model in:**

```rust
#[derive(Model)]
#[model(app = Vehicles)]   // type reference, not a string
pub struct Vehicle { ... }
```

Models without `#[model(app = ...)]` default to the global bucket (`app_label = ''`). Users who
never invoke `djogi::apps!` see no change at all — the flat layout from the current spec is
fully preserved as the default.

**On-disk layout with apps:**

```
migrations/
├── 0001_initial_up.sql              <- global (flat, unchanged from current spec)
├── 0001_initial_down.sql
├── vehicles/
│   ├── 0001_initial_up.sql
│   ├── 0001_initial_down.sql
│   └── schema_snapshot.json
└── users/
    ├── 0001_initial_up.sql
    ├── 0001_initial_down.sql
    └── schema_snapshot.json
```

**Cross-app FK dependency inference:** Because `Model::App` is an associated type (a concrete
ZST at compile time), the differ resolves `ForeignKey<users::User>` to `<users::User as
Model>::App = apps::Users` without runtime lookups. Cross-app dependency edges are computed
from the type graph. The topological sort at apply time uses this graph directly — no manual
`dependencies = [...]` declarations required. This is the key advantage over Django's app
dependency model.

**Four lifecycle operations:**

| Operation | Declaration | Migration effect | Ledger effect |
|---|---|---|---|
| **Add** | Add variant to `djogi::apps!` | None until a model is attached and changed | Normal INSERT on first migration |
| **Rename** | `#[app(renamed_from = "old_label")]` on new variant | SQL: `UPDATE djogi_schema_migrations SET app_label = 'new' WHERE app_label = 'old'`; `compose` performs `git mv` of the folder | UPDATE (the one exception to append-only ledger) |
| **Remove** | `#[app(tombstone)]` on retiring variant | Generates destructive migration dropping all tables in the app; gated by `--allow-destructive` | Normal INSERT with retiring `app_label` |
| **Move model** | `#[model(moved_from_app = "old_label")]` on the model | SQL no-op marker migration in new app's folder; both snapshots updated at `compose` time | Normal INSERT with marker description |

All four markers are migration-window-only — removed after the migration applies. The differ
detects stale markers and emits a hard error (same lifecycle as `#[field(renamed_from)]`,
per R-20).

**Snapshot extension:** Each per-app `schema_snapshot.json` gains a `registered_apps` field:

```json
{
  "format_version": 1,
  "registered_apps": ["vehicles", "users", "orders"],
  "models": { ... }
}
```

### 2.6 Drift Detection (New — D-Codes)

The drift detection system uses structured diagnostic codes. Codes are emitted from two surfaces:
`build.rs` (every build, must be terse) and the library `djogi::migrate::verify`
entry point (explicit invocation, can be rich; also available as `djogi migrations verify`).

| Code | Meaning | Fires at |
|---|---|---|
| D001 | Schema drift — model descriptors diverge from snapshot | `build.rs`, `verify` |
| D002 | Destructive migration requires `--allow-destructive` | `compose` |
| D003 | Schema drift extended (includes new field detail) | `build.rs`, `verify` |
| D004 | App folder drift — filesystem folders differ from `registered_apps` in snapshot | `build.rs`, `verify` |
| D008 | Stale rename annotation — snapshot already reflects the rename | `compose` |
| D010 | Unknown `app_label` in ledger — label not in `AppRegistry` or snapshot | `verify` (warn), `apply` (error) |
| D011 | Model-app mismatch — model's `App` type differs from snapshot-recorded app | `verify` |
| D020 | Submodule has uncommitted changes | `pull` (refuse; `--force` overrides) |
| D021 | Submodule has unpushed local commits | `pull` (refuse; `--force` overrides — destroys them) |
| D022 | `migrations/` is not configured as a submodule | `pull` (hard fail; user runs `git submodule add` once manually) |
| D023 | Git fetch failed (auth or network) | `pull` |
| D024 | Parent repo is dirty and `--fetch-parent` was requested | `pull` (not overridable — commit/stash first) |
| D025 | `.djogi-migrations-lock` held by another invocation | `pull`, `apply`, `compose`, `repair` (30s timeout) |

Override path at apply time: `--force-apply` (discouraged; writes an `orphan_handled` audit row).
Standard reconciliation: run `djogi migrations verify`, then the relevant `djogi migrations repair <subcommand>` (or, from code, `djogi::migrate::verify` and the `djogi::migrate::repair_*` helpers).

The `build.rs` surface emits plain `cargo:warning=djogi: ...` strings only. No spans, no ANSI
codes — rustc does not expose rich diagnostic APIs from `build.rs` on stable. Rich colored output
(via `owo-colors`/`termcolor`) is reserved for `djogi migrations *` subcommands, which
have full TTY control. (Decision record Part I, R-12 rationale)

### 2.7 Transaction Boundaries

Every migration runs inside `BEGIN`/`COMMIT` by default. Postgres's DDL-in-transaction support
makes this safe for `CREATE TABLE`, `ALTER TABLE`, and all other schema operations except the
handful that Postgres explicitly forbids inside a transaction.

**The `-- djogi:no-transaction` directive:**

- Must appear on the first non-blank, non-comment line of the SQL file.
- Placement anywhere else is a parse error, not silent acceptance.
- When present, the runner treats the entire file as a single non-transactional segment.
- Each of the `_up.sql` and `_down.sql` files carries its own directive; they are evaluated
  independently.
- When the differ determines that a migration contains non-transactional operations (e.g.,
  `CREATE INDEX CONCURRENTLY`), the generator emits the directive automatically.
- If the runner detects a statement that Postgres would reject inside a transaction (e.g.,
  `CREATE INDEX CONCURRENTLY`) in a file without the directive, it emits a hard error before
  execution (R-08).

**Ledger behavior under non-transactional migrations:** The `status = 'pending'` INSERT is
committed before DDL begins (because DDL cannot be inside a transaction). The INSERT remains
visible as an "in-flight" marker. After all statements succeed, the runner UPDATEs to
`status = 'applied'`. Each auto-committed statement increments `applied_steps_count`. (R-06)

### 2.8 Partial-Apply Recovery

When a non-transactional migration fails mid-way (statements 1..N auto-committed, statement
N+1 failed), the runner:

1. Updates the ledger row: `status = 'failed'`, `applied_steps_count = N`, `total_steps = M`.
2. Writes `migrations/.migration_failure.json`:
   ```json
   {
     "failed_version": "0009_add_payment_index",
     "failed_segment": 2,
     "failed_at": "2026-04-22T10:30:00Z",
     "expected_next_snapshot_version": "0009"
   }
   ```
3. Refuses to plan or apply further migrations until the marker is cleared by
   `djogi migrations repair`.

The marker file is the blocking signal — not the ledger row alone — because the runner may not
be able to connect to the database in a post-crash state, but it can always read the local
filesystem (R-07).

`djogi migrations repair` removes the marker, prompts the operator for confirmation, and
transitions the ledger row to `status = 'applied'` (if the partial apply is complete and safe)
or `status = 'rolled_back'` (if the operator rolled back manually). The `partial_apply_note`
column records the operator's explanation.

### 2.9 Keeping Local in Sync with the Submodule

Because `migrations/` is a git submodule updated by CI (§10.5), every developer's working copy
periodically falls behind whenever a teammate's migration is merged upstream. `djogi migrations
pull` is the ergonomic wrapper around the `git submodule` incantation, positioned so developers
don't need to remember the raw git commands or accidentally desync their submodule pointer.

**What `pull` does, in order:**

1. Preflight checks (refuse early, with specific diagnostics):
    - `migrations/` is configured as a submodule (else `D022`).
    - No uncommitted changes inside `migrations/` (else `D020`, overridable with `--force`).
    - No unpushed commits inside `migrations/` (else `D021`, overridable with `--force`).
    - Parent repo is clean if `--fetch-parent` is set (else `D024`; not overridable — user must
      commit or stash parent-level changes first).
2. Optional: `git pull --ff-only` on the parent repo (only if `--fetch-parent` given). Fast-forward
   only — never creates merge commits on the parent from a Djogi command.
3. `git submodule update --init --recursive migrations` — aligns the submodule working tree with
   the parent's recorded pointer; fetches from the submodule's remote as needed.
4. Reports the file-level diff: which migration files were added or updated.
5. Chains automatically into `djogi migrations status` (unless `--no-status`), so the developer
   sees immediately what is now pending against their local DB.
6. Optional: chains into `djogi migrations apply` (only if `--apply` given).

**What `pull` deliberately does NOT do:**

- No implicit `git pull` on the parent repo. Parent-level state changes require explicit opt-in
  via `--fetch-parent`. Consistent with the "conservative default, explicit override" ethos.
- No implicit `migrations apply` after a successful pull. File fetch is reversible; DB mutation
  is not. Two separate decisions, two separate commands; `--apply` opt-in if the developer wants
  the combined flow.
- No `--branch` or `--commit` flag. The submodule has a configured tracked branch; we honour it.
  Arbitrary commit checkout is a git-level operation.
- No `migrations push` counterpart. Writing to the submodule is CI's job; developers push
  migrations upstream through the parent-repo PR flow, with CI owning the submodule commit step.
- No multi-submodule support. Djogi assumes exactly one `migrations/` submodule (§10.5).

**Locking.** `djogi migrations {pull, apply, compose, repair}` all acquire an `fcntl` advisory
lock on `.djogi-migrations-lock` at the parent repo root. This prevents concurrent invocations
on the same machine from racing on migration files. Timeout 30 seconds (matches the Postgres
advisory-lock timeout from R-03 for consistency); failure surfaces as `D025`. This file-level
lock is separate from the Postgres advisory lock (R-03) — the Postgres lock protects concurrent
apply across *machines*; this fcntl lock protects concurrent CLI invocations on *one* machine.

**Dry-run semantics.** `--dry-run` fetches from the submodule remote (network required) so it
can report the actual diff; it just doesn't write to the working tree. Consistent with `verify`
and `status` — Djogi's read-only commands are maximally informative.

**Output is coloured** via `owo-colors` / `termcolor`, respecting `NO_COLOR`. Commit hashes,
file paths, and pending-migration names are highlighted. Tier-two principle from R-12 — `djogi`
subcommands can afford rich output, `build.rs` cannot.

---

## Part III: Lifecycle Walkthrough (Worked Examples)

These examples show the CLI flow end-to-end. They are the most persuasive section for a team
review — they demonstrate what the proposed system feels like in practice.

### 3.1 Greenfield: first model, first migration

```rust
// src/vehicles/models.rs
#[derive(Model)]
pub struct Vehicle {
    pub make: String,
    pub model_year: i32,
}
```

```
$ cargo build
warning: djogi: schema drift detected — run `djogi migrations compose`

$ djogi migrations compose
Generated:
  migrations/0001_initial_up.sql
  migrations/0001_initial_down.sql

Review the SQL, then apply when ready.

$ cat migrations/0001_initial_up.sql
-- Migration: 0001_initial
-- Direction: UP
-- Generated: 2026-04-22T10:00:00Z
-- Execution-Mode: transactional
-- Snapshot-Base: (none)

CREATE TABLE vehicles (
    id           BIGINT NOT NULL PRIMARY KEY DEFAULT heerid_next_desc(),
    make         TEXT   NOT NULL,
    model_year   INTEGER NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

$ djogi migrations apply
Acquiring advisory lock 4994068948568834898...ok
Applying 0001_initial...ok (34ms)
Snapshot updated: migrations/schema_snapshot.json

$ cargo build
(silent — no drift)
```

File-state transitions: `target/djogi_models.json` written by proc macro → `compose` reads it,
writes SQL files and `target/djogi_pending/global.json` → `apply` consumes SQL, updates
`migrations/schema_snapshot.json`, deletes `target/djogi_pending/global.json`.

### 3.2 Adding a field

```rust
// Add to Vehicle struct:
pub horsepower: i32,
```

```
$ cargo build
warning: djogi: schema drift detected — run `djogi migrations compose`

$ djogi migrations compose
Generated:
  migrations/0002_add_vehicle_horsepower_up.sql
  migrations/0002_add_vehicle_horsepower_down.sql

$ cat migrations/0002_add_vehicle_horsepower_up.sql
-- Migration: 0002_add_vehicle_horsepower
-- Direction: UP
-- Execution-Mode: transactional

ALTER TABLE vehicles ADD COLUMN horsepower INTEGER NOT NULL DEFAULT 0;

$ djogi migrations apply
Acquiring advisory lock...ok
run_id: 7823456789012345678
Applying 0002_add_vehicle_horsepower...ok (12ms)
Snapshot updated.
```

### 3.3 Dropping a column (destructive path)

```rust
// Remove horsepower from Vehicle struct
```

```
$ djogi migrations compose
error[D002]: destructive migration requires confirmation
  = DROP COLUMN horsepower — data is irrecoverable on rollback
  = help: run with --allow-destructive to generate

$ djogi migrations compose --allow-destructive
Generated:
  migrations/0003_drop_vehicle_horsepower_up.sql
  migrations/0003_drop_vehicle_horsepower_down.sql

$ cat migrations/0003_drop_vehicle_horsepower_up.sql
-- Migration: 0003_drop_vehicle_horsepower
-- Direction: UP
-- Execution-Mode: transactional

-- DJOGI WARNING: DROP COLUMN horsepower — data in this column will be permanently
--   destroyed. Rollback restores the column definition but not the data.
ALTER TABLE vehicles DROP COLUMN horsepower;
```

The warning comment lands in the UP file where code reviewers see it, not just in the DOWN file.
(R-19)

### 3.4 Non-transactional migration (CREATE INDEX CONCURRENTLY)

```rust
// Add an index on vehicles.make flagged as concurrent:
#[model(indexes(
    index(fields = [make], name = "vehicles_make_idx", concurrently = true)
))]
pub struct Vehicle { ... }
```

```
$ djogi migrations compose
Generated:
  migrations/0004_add_vehicles_make_idx_up.sql
  migrations/0004_add_vehicles_make_idx_down.sql

$ cat migrations/0004_add_vehicles_make_idx_up.sql
-- Migration: 0004_add_vehicles_make_idx
-- Direction: UP
-- Execution-Mode: non_transactional
-- djogi:no-transaction

CREATE INDEX CONCURRENTLY vehicles_make_idx ON vehicles (make);

$ djogi migrations apply
Acquiring advisory lock...ok
run_id: 7823456789012345679
Applying 0004_add_vehicles_make_idx (non-transactional)...
  step 1/1: CREATE INDEX CONCURRENTLY... ok (2341ms)
applied_steps_count: 1/1
Snapshot updated.
```

The ledger row for this migration has `execution_mode = 'non_transactional'`, `total_steps = 1`,
`applied_steps_count = 1`.

Because the down file is non-transactional, `djogi migrations rollback` refuses this migration with
exit code `2`; undo it through the `djogi::migrate::rollback_plan` library entry point or by running
`DROP INDEX CONCURRENTLY` by hand. See [the migrations guide](../guide/migrations.md) for the
rollback-refusal contract.

### 3.5 Partial failure and repair

Scenario: a non-transactional migration with two steps; step 2 fails.

```
$ djogi migrations apply
Acquiring advisory lock...ok
run_id: 7823456789012345680
Applying 0005_add_two_indexes (non-transactional)...
  step 1/2: CREATE INDEX CONCURRENTLY vehicles_vin_idx... ok (1200ms)
  step 2/2: CREATE INDEX CONCURRENTLY vehicles_active_idx... FAILED
    detail: column "active" does not exist

error[M007]: migration 0005_add_two_indexes failed at step 2/2
  = applied_steps_count: 1
  = help: vehicles_vin_idx was created (step 1 committed)
  = help: run `djogi migrations repair resume-partial 0005_add_two_indexes` to replay the remaining step (or `djogi migrations repair partial-apply` to resolve the row by hand) before applying further migrations

Wrote: migrations/.migration_failure.json
```

```
$ djogi migrations apply
error: migration failure marker present — resolve before applying
  = help: run `djogi migrations repair resume-partial` (or `repair partial-apply`) to resolve

$ djogi migrations repair partial-apply 0005_add_two_indexes applied
Found failure: 0005_add_two_indexes at step 2/2
  Step 1 committed: CREATE INDEX CONCURRENTLY vehicles_vin_idx

Options:
  [a] Mark step 2 as manually applied (if you ran it by hand)
  [r] Mark entire migration as rolled back (if you dropped vehicles_vin_idx manually)
  [q] Quit without changing anything

Choice: a
Note (optional): ran step 2 manually after fixing the schema
Ledger updated: status=applied, applied_steps_count=2
Marker file cleared.
```

### 3.6 Renaming an app

```rust
// Rename the app variant in djogi::apps!
djogi::apps! {
    #[app(renamed_from = "vehicles")]
    Fleet,   // was Vehicles
    Users,
    Orders,
}
```

```
$ djogi migrations compose
Generated:
  migrations/fleet/0002_rename_app_from_vehicles_up.sql
  migrations/fleet/0002_rename_app_from_vehicles_down.sql
  (git mv migrations/vehicles/ migrations/fleet/ — performed automatically)

$ cat migrations/fleet/0002_rename_app_from_vehicles_up.sql
-- Migration: fleet/0002_rename_app_from_vehicles
-- Direction: UP
-- Execution-Mode: transactional

UPDATE djogi_schema_migrations
    SET app_label = 'fleet'
    WHERE app_label = 'vehicles';

$ djogi migrations apply
Applying fleet/0002_rename_app_from_vehicles...ok

$ # After apply: remove #[app(renamed_from = "vehicles")] from the macro.
$ # The differ will error if you forget (stale annotation detection).
```

### 3.7 Moving a model to a different app

```rust
#[derive(Model)]
#[model(
    app = Orders,
    moved_from_app = "fleet"   // was in Fleet app
)]
pub struct Shipment { ... }
```

```
$ djogi migrations compose
Generated:
  migrations/orders/0003_move_shipment_from_fleet_up.sql
  migrations/orders/0003_move_shipment_from_fleet_down.sql
  (both fleet and orders schema_snapshot.json updated)

$ cat migrations/orders/0003_move_shipment_from_fleet_up.sql
-- Migration: orders/0003_move_shipment_from_fleet
-- Direction: UP
-- Execution-Mode: transactional
-- Note: SQL no-op. The shipments table is unchanged on disk.
--   This migration records the organizational move in the ledger.

SELECT 1; -- marker

$ djogi migrations apply
Applying orders/0003_move_shipment_from_fleet...ok (marker migration)

$ # Remove #[model(moved_from_app = "fleet")] after apply.
```

The underlying `shipments` table is unchanged. The ledger records the move. The organizational
grouping shifts; no data migration is required.

### 3.8 Adopting an existing database (baseline)

Team has an existing Postgres database with tables already matching what Djogi's models declare.
No migration history in the DB yet.

```
$ # Project the live schema into a baseline row + snapshot.
$ # No compose needed — baseline reads the live catalog directly.
$ djogi migrations baseline V20260422100000__baseline --reason "existing DB adoption"
djogi migrations baseline: established baseline `V20260422100000__baseline` (ledger_id=...) in 0.1s

$ # Do NOT apply — the tables already exist and are now captured in the snapshot.

$ djogi migrations verify
Comparing snapshot against live DB...
  OK: all 4 tables match snapshot
  OK: all 12 columns match snapshot
  OK: all 3 indexes match snapshot
Exit 0 — live DB matches snapshot exactly.
```

The baseline flow uses the same advisory lock as the library apply path. It projects the live
Postgres catalog into a single ledger row with `status = 'baseline'` (no SQL runs against user
tables), computes `checksum_up` as a content-addressed hash of that projection, and persists the
projection as the bucket's canonical snapshot. Future `migrations compose` and library `apply_plan`
invocations see a clean starting point and generate only truly new migrations. (R-10, OI-03)

### 3.9 Detecting out-of-band DB tampering

Scenario: an operator ran `ALTER TABLE vehicles ADD COLUMN legacy_id TEXT` directly on the
production DB. No migration was generated or applied.

```
$ djogi migrations verify
Comparing snapshot against live DB...
  WARN: live DB has column vehicles.legacy_id — not in snapshot
  OK: all other 11 columns match snapshot
  OK: all 3 indexes match snapshot
Exit 1 — 1 discrepancy found.

$ # Options:
$ # 1. Generate a migration that formally adopts legacy_id into the schema model.
$ # 2. Drop the column from the DB and re-add it through a proper migration.
$ # 3. Document the discrepancy and use --force-apply if circumstances require it.
```

`djogi::migrate::verify` is the operational checkpoint that catches out-of-band drift
without requiring a shadow DB or live introspection at every build. It compares
`migrations/schema_snapshot.json` against `information_schema` and `pg_catalog`. (R-24)

### 3.10 Developer pulls after CI merges a teammate's migration

Scenario: the developer has been heads-down on a feature branch. Meanwhile, `main` merged and
CI pushed three new migrations to the `migrations/` submodule. The developer returns, pulls the
parent repo, and wants to catch up their local DB in one shot.

```
$ git pull                                  # parent repo fast-forwards
Updating a1b2c3d..e4f5g6h
 migrations | 3 files changed, 128 insertions(+)

$ djogi migrations pull --apply
Fetching migrations submodule...
  remote:   https://github.com/acme/djogi-migrations.git (branch: main)
  previous: a1b2c3d  →  new: e4f5g6h

Files added:
  migrations/vehicles/0004_add_mileage_up.sql
  migrations/vehicles/0004_add_mileage_down.sql
  migrations/vehicles/0005_add_vin_index_up.sql
  migrations/vehicles/0005_add_vin_index_down.sql
  migrations/users/0002_add_email_verified_up.sql
  migrations/users/0002_add_email_verified_down.sql
  migrations/vehicles/schema_snapshot.json        (updated)
  migrations/users/schema_snapshot.json           (updated)

Status:
  Applied:  3 migrations
  Pending:  3 migrations
    - vehicles/0004_add_mileage
    - vehicles/0005_add_vin_index
    - users/0002_add_email_verified

Applying pending migrations (--apply)...
  [1/3] vehicles/0004_add_mileage           ✓ applied in 12ms
  [2/3] vehicles/0005_add_vin_index         ✓ applied in 34ms  (CREATE INDEX CONCURRENTLY)
  [3/3] users/0002_add_email_verified        ✓ applied in 8ms

Local database is up to date.
```

If the pull encounters a snag — say the submodule has uncommitted local changes from a prior
experiment — the command refuses with a specific diagnostic rather than clobbering work:

```
$ djogi migrations pull
error[D020]: migrations submodule has uncommitted changes
  path: migrations/
  modified: ['vehicles/0003_wip_temp.sql']

  note: migrations/ is managed by CI — direct edits are unusual and usually a mistake.

  Resolution:
    (a) commit the changes inside migrations/ (dangerous — CI owns this folder)
    (b) reset: git -C migrations checkout -- .
    (c) --force: discard local changes (destructive)
```

This mirrors the conservative-by-default philosophy: pull is non-destructive by construction;
anything that looks risky requires explicit override. (§2.9, R-12)

---

## Part IV: What Changes vs Existing Plan

### 4.1 Supersessions (existing docs that get updated on acceptance)

| Document | Section | Change |
|---|---|---|
| `docs/spec/migrations.md` | §10.1 | Remove "sqlx's built-in runner", remove `_sqlx_migrations`, add runner ownership statement |
| `docs/spec/migrations.md` | §10.1 | Add checksum algorithm subsection (SHA-256, `V1:` prefix, content-only) |
| `docs/spec/migrations.md` | §10.2 | Rewrite build.rs behavior to diagnostic-only; add three-way match logic |
| `docs/spec/migrations.md` | §10.2 | Add out-of-order policy tiers (dev: allow + warn; CI/prod: reject default) |
| `docs/spec/migrations.md` | §10.2 | Add rollback ordering rule (by `id` column, temporal not version-string) |
| `docs/spec/migrations.md` | §10.3 | Add `format_version` field, merge-conflict resolution workflow |
| `docs/spec/migrations.md` | §10.3 | Add `.migration_failure.json` marker file protocol |
| `docs/spec/migrations.md` | §10.4 | Add `-- djogi:no-transaction` directive specification |
| `docs/spec/migrations.md` | §10.4 | Add `-- DJOGI WARNING:` comment format for UP files |
| `docs/spec/migrations.md` | §10.6 | Rewrite `SchemaDelta` enum with complete Phase 7 variant list |
| `docs/spec/migrations.md` | §10.7 (new) | Finalized ledger DDL |
| `docs/spec/migrations.md` | §10.7 (new) | Advisory lock key and derivation |
| `docs/spec/migrations.md` | §10.7 (new) | Pre-write row pattern |
| `docs/spec/migrations.md` | §10.8 (new) | Baseline and fake adoption flows |
| `docs/spec/migrations.md` | §10.9 (new) | `verify` command and live-DB comparison |
| `docs/spec/migrations.md` | §10.10 (new) | Apps subsystem — macro, lifecycle, snapshot extension |
| `docs/spec/decisions.md` | "Build drift diagnostic" | Re-written: diagnostic-only, not file-generating |
| `docs/spec/decisions.md` | "Migration generation" | Re-written: explicit `migrations compose`, not auto-via-`build.rs` |
| `docs/spec/decisions.md` | New rows (approx. 14) | Advisory lock key, ledger table name, checksum algorithm, runner ownership, rollback order, composite naming, rename lifecycle, etc. |
| Phase 7 v2 plan | §Ledger shape | Replace draft DDL with finalized DDL from §2.3 |
| Phase 7 v2 plan | §CLI Surface | Replace `makemigrations`/`migrate` nomenclature with `migrations compose`/`migrations apply` |

### 4.2 Additions (new concepts not in existing plan)

**Apps subsystem** (`djogi::apps!` macro, `#[model(app = ...)]`, per-app migration folders,
four lifecycle operations, compile-time FK graph): entirely new. See §2.5.

**Drift detection D-codes** (D001–D011): structured taxonomy with precise fire-conditions.

**Partial-apply structured counter** (`applied_steps_count`, `total_steps`): queryable per-step
progress tracking replacing free-text `partial_apply_detail`.

**`run_id` as HeerId**: HeerId-typed deployment group column, one per `migrations apply` run.

**Snapshot `format_version`**: explicit format-versioning for forward-compatible snapshot evolution.

**`HistoryDiagnostic` taxonomy** (`DatabaseIsBehind`, `UnexpectedHistory`, `HistoryDiverged`):
structured plan/status output for CI integration.

**`IndexSpec` extensions** (partial index `where_clause`, functional index `expression`, JSONB
path `json_path`): descriptor-level support for patterns no surveyed Rust migration system handles.

**`NULLS NOT DISTINCT` reservation**: syntax space reserved in `#[model(indexes(unique(...,
nulls_not_distinct = true)))]` but not implemented until v0.2.

### 4.3 Two Re-Opened Locked Decisions

This is the only section of the proposal that contradicts currently-locked team decisions.

The two rows in `docs/spec/decisions.md` that get re-opened and re-written:

**"Build drift diagnostic"**
- Current locked text: "Compiler-style `note` (not error) — migration generated, build continues,
  developer reviews"
- Proposed text: "Plain cargo warning on drift — `build.rs` is diagnostic-only. Migration file
  generation requires explicit `djogi migrations compose` invocation."

**"Migration generation"**
- Current locked text: "Automatic via `build.rs` on drift detection — generates pair, build
  continues"
- Proposed text: "`build.rs` detects drift and emits warning. Files generated only by
  `djogi migrations compose`. `build.rs` never writes to `migrations/` or any submodule."

**Why the re-open is the right call:** The Phase 7 design document already states "`build.rs`
may read the snapshot. It must never mutate it." The principle extends: if `build.rs` should not
mutate the snapshot (applied-state truth), it should not mutate migration files (the review
surface) either. `migrations/` is a git submodule; `build.rs` writing to a submodule without
developer review is the wrong default. IDE churn from directory watchers re-triggering on every
`cargo build` is a concrete developer-experience cost. The diagnostic-only model is cleaner, more
consistent with the Phase 7 design's stated invariants, and does not reduce safety — the cargo
warning is just as visible as a generated file appearing in the editor.

If the team rejects this re-open, Pillars 1–5 and 7–10 are unaffected. Only the `build.rs`
behavior and the two spec rows change. The CLI command names remain `migrations compose` and
`migrations apply` regardless.

### 4.4 What Stays the Same

This is a long list, intentionally so. The proposal is additive on top of a stable foundation.

- Descriptor-first architecture (`#[model]` structs as desired-state source of truth)
- `migrations/` as a git submodule, pipeline-managed
- Paired `_up.sql` / `_down.sql` files as the primary review artifact
- Postgres 18+ exclusively
- HeerIdRecencyBiased as the default PK (`BIGINT DEFAULT heerid_next_desc()`)
- No regex anywhere in the codebase or documentation
- Explicit field rename via `#[field(renamed_from = "old_name")]`
- Explicit table rename via `#[model(renamed_from = "old_table")]`
- `RESTRICT` as the FK cascade default
- `schema_snapshot.json` updated only on successful `migrations apply` — never by `build.rs`
- Advisory locking before reading the pending set
- Per-migration transaction as the default
- Paired up/down file generation for all schema operations
- Composite unique constraints via `ALTER TABLE ... ADD CONSTRAINT UNIQUE` (not `CREATE UNIQUE INDEX`)
- Composite index auto-naming: `<table>_<col1>_<col2>_key` / `_idx` with SHA-256 truncation
  for names exceeding Postgres's 63-byte identifier limit
- `build.rs` emits a compiler-style diagnostic (not an error) — the note vs. warning distinction
  is the only change: note becomes warning, file-generation is removed
- Out-of-order migrations: dev allows, CI/prod rejects by default
- Rollback restores schema shape; data restoration is backup/restore territory
- No shadow DB
- Phase 7 task sequence (T1–T8) unchanged

---

## Part V: Risk Assessment

### 5.1 Battle-tested patterns adopted

Every pattern below has production track record in at least one of the 11 surveyed systems.
The risk from adopting them is low — the behavior is well-understood and the failure modes are
documented.

**Per-migration transaction (9 of 11 applicable systems):** DDL inside `BEGIN`/`COMMIT` is the
Postgres default safety net. The one exception in the survey (refinery) is documented as the
largest operational gap in the Rust ecosystem (T05).

**Name-based column matching (all autogenerate systems):** Matching columns by name rather than
position means struct field reordering produces no migration. No system in 40+ years of SQL
tooling has shipped positional matching (doc 15, C-04).

**Advisory lock + session scope (Flyway, Prisma):** Session-scoped `pg_try_advisory_lock` is
the Postgres-native concurrency primitive. It auto-releases on TCP teardown, requires no cleanup
table, and has been in production at Flyway scale for years (T04, R-03).

**Two-bucket destructive classifier (Prisma pattern, escalated):** Prisma's `unexecutableSteps`/
`warnings` split has been in production for approximately five years. Djogi escalates two
operations to `unexecutableSteps`; the escalation is conservative, not radical (T09, R-18).

**Format-versioned checksums (Liquibase):** Liquibase's `V:hex` format has been in production
since approximately 2010. The V1: prefix is a straightforward adaptation to SHA-256 (T03, R-05).

**Explicit repair + baseline + verify:** Flyway (`repair`), Prisma (`migrate resolve`), Django
(`--fake`) all demonstrate that these flows are needed by real teams. No production system
that shipped repair tooling has regretted it (doc 15, C-08).

### 5.2 Novel-to-Djogi patterns (higher scrutiny warranted)

These patterns have precedent in pieces, but not as a combined package in any surveyed system.

**Compile-time cross-app FK dependency graph:** No surveyed system resolves cross-app FK
dependencies via the type system at compile time. Django resolves them at runtime via string app
labels; Prisma has no app concept. This is genuinely novel. The risk is that the implementation
hits edge cases the type system cannot express cleanly (circular FK dependencies across apps,
forward-declared types). The payoff is that dependency errors become compile errors, not runtime
surprises.

**Noun-grouped CLI convention (`migrations compose`, `migrations apply`):** Standard in many
CLIs (`git remote add`, `kubectl get pods`) but unusual in the Rust migration ecosystem. The
risk is team familiarity — anyone who has used Django will reach for `makemigrations`. The payoff
is discoverability: `djogi migrations <tab>` reveals the full surface.

**`V1:` checksum prefix adopted from Liquibase into a Rust/SHA-256 context:** Liquibase uses
`V:` with MD5. Adapting the format-versioning concept to SHA-256 and Rust's `sha2` crate is
straightforward. The risk is near-zero — it is the format that matters, not the algorithm that
Liquibase chose.

**Per-app pending snapshots in `target/djogi_pending/<app>.json`:** The user's instinct (separate
folder, per-app files) is the right design for clarity, but no surveyed system uses exactly this
layout. The risk is tooling edge cases in the pending-file lifecycle (e.g., stale pending files
after a branch switch). The three-way match logic in `build.rs` is the mitigation.

### 5.3 Deferred complexity

These items were considered, scoped out for v0.1, and explicitly documented. They represent
known limitations, not omissions.

**P2 — Online-safe mode with automatic CONCURRENTLY injection (R-28):** No surveyed system
fully automates zero-downtime DDL. Phase 7.5 is the planned home for the five staged live-migration
patterns. v0.1 operators who need `CREATE INDEX CONCURRENTLY` hand-edit the generated SQL and
add `-- djogi:no-transaction` (five minutes of work).

**P2 — Shadow DB drift detection via a future `--live` flag (R-29):** `djogi::migrate::verify`
provides snapshot-vs-live comparison. A full shadow-DB approach (Prisma) requires `CREATE DATABASE`
permission and a full schema replay per diff. Deferred to v0.2.

**P2 — `NULLS NOT DISTINCT` index modifier (R-27):** Syntax space reserved in the attribute
grammar; implementation deferred to v0.2.

**P2 — `djogi reconcile` for automated post-merge snapshot conflicts (R-30):** The
manual workflow (`djogi migrations compose` after merge) is documented. The automated
command is a v0.2 convenience.

**Deferred outside Phase 7 entirely:** composite primary keys, Rust data migrations with
historical model reconstruction, `inspectdb` reverse-introspection, migration squashing.

---

## Part VI: Adoption Plan

### 6.1 If the team approves as-is

1. Update `docs/spec/migrations.md` §10 per the supersession table in §4.1. This is primarily
   rewriting §10.1–§10.6 and adding §10.7–§10.10.
2. Update `docs/spec/decisions.md` — re-write the two re-opened rows (§4.3); add approximately
   14 new rows for the newly-locked decisions (advisory lock key, checksum algorithm, ledger
   table name, runner ownership, rollback ordering, app lifecycle semantics, etc.).
3. Amend the Phase 7 v2 plan to reference the updated spec: replace the draft DDL, update the
   CLI surface section, note the apps subsystem as a new T1 sub-task.
4. Implementation proceeds against the finalized spec via Phase 7 T1–T8 task sequence.
5. The research artifacts in `docs/research/migrations/2026-04-22/` remain as-is — they are the
   audit trail, not the active spec.

### 6.2 If team wants modifications

The following pillars are highest-cost-to-change. Re-opening them requires new research sweeps
or significant design re-work:

- **Pillar 1 (descriptor-first + side-car snapshot):** Re-opening this is effectively re-doing
  the source-of-truth (T01) and diff-algorithm (T11) topic research. This is an architectural
  decision baked into `build.rs`, the differ, and the runner.
- **Pillar 2 (Djogi-owned runner):** Already locked in Phase 7 design and v2 plan. Re-opening
  requires justifying why `sqlx::migrate`'s missing columns, missing advisory lock, and missing
  non-transactional segment awareness are acceptable for Djogi's requirements.
- **Pillar 7 (apps subsystem):** The compile-time sealed enum and cross-app FK graph inference
  require careful design; changes to the macro expansion affect `djogi-macros`, the differ, the
  runner, and the snapshot format simultaneously.

The following pillars are refinements — cheap to modify without cascading re-work:

- **Pillar 6 (build.rs diagnostic-only):** If the team accepts file-generation from `build.rs`,
  the rest of the design is unaffected. The `build.rs` three-way match logic still applies;
  the pending file lifecycle is unchanged. The git-submodule concern and IDE-churn concern
  are the only costs of keeping the old behavior.
- **Pillar 9 (lifecycle markers):** The specific attribute names (`moved_from_app`, `tombstone`)
  are naming choices. Re-naming them does not affect the underlying mechanism.
- **Pillar 10 (repair + baseline + verify + status):** The scope of these commands can be
  adjusted. The only cross-cutting concern is that `verify` depends on the drift-detection
  taxonomy (§2.6), and `repair` depends on the partial-apply structured counter (§2.8).

### 6.3 If team rejects specific items

If the team rejects any decision that was surfaced as an open item during the research walkthrough
(OI-01 through OI-06), that item becomes open again and requires another decision round. The
six open items are now all locked (per decision record Part VI); their re-opening would delay
Phase 7 T1.

If the team rejects the apps subsystem (Pillar 7) entirely, the rest of the proposal is
unaffected. The ledger DDL loses `app_label`. The snapshot format loses `registered_apps`. The
`SchemaDelta` enum loses `RenameApp`, `TombstoneApp`, and `MoveModel`. The drift detection codes
D004/D010/D011 are removed. Everything else stands.

If the team rejects the R-12 re-open (keeps `build.rs` as file-generating), the two re-opened
decision rows in `docs/spec/decisions.md` stay as currently written. The CLI names, ledger DDL,
checksum format, advisory lock key, and apps subsystem are unaffected.

---

## Appendix A: Decision-to-Research Citations

One row per locked decision. Every claim in the proposal is auditable to a topic-level document.

| Decision | Lock location | Primary citation |
|---|---|---|
| Runner is Djogi-owned, not `sqlx::migrate` | Decision record C-01; R-01 | T12 §Ecosystem contrast; T04 §Advisory locks |
| Ledger table: `djogi_schema_migrations` | Decision record C-02; R-02 | Phase 7 v2 plan §Ledger shape |
| `SchemaDelta` enum — complete Phase 7 variant list | Decision record C-03; R-11 | Phase 7 v2 plan §Canonical Scope |
| Advisory lock key: `0x444A4F474D494752` | Decision record OI-01 subset; R-03 | T04 §Key derivation strategies |
| `down_checksum` NULL only when `_down.sql` absent | Decision record OI-02 | R-05 §Repair semantics |
| `applied_at = now()` for baseline/faked rows | Decision record OI-03 | T06 §Baseline/fake/stamp semantics |
| Pending snapshot: `target/djogi_pending/<app>.json` | Decision record OI-04 | T11 §Snapshot model; doc 14 §OI-04 |
| `applied_steps_count` + `total_steps` structured counter | Decision record OI-05 | T02 §Partial-apply tracking; Prisma pattern |
| Snapshot written exactly once, atomic rename | Decision record OI-06 | T01 §Snapshot invariant |
| SHA-256 checksum, `V1:` prefix, content-only | Decision record R-05 | T03 §Algorithm landscape |
| `status = 'pending'` pre-write row pattern | Decision record R-06 | T02 §Status and failure flags |
| `.migration_failure.json` marker file | Decision record R-07 | T01 §Open design gap; T05 §Non-transactional |
| `-- djogi:no-transaction` directive spec | Decision record R-08 | T05 §Approaches |
| Out-of-order policy: env-sensitive | Decision record R-09 | T06 §Out-of-order problem; Phase 7 design |
| Baseline and fake: first-class flows | Decision record R-10 | T06 §Comparison matrix |
| Rollback ordering by `id` (temporal) | Decision record R-13 | T06 §Rollback ordering |
| `#[model(indexes(...))]` attribute syntax | Decision record R-14 | T08 §Representation per system |
| Composite naming: `<table>_<cols>_key/_idx` | Decision record R-15 | T08 §Naming convention |
| `#[model(renamed_from)]` locked | Decision record R-16 | T07 §Rename detection comparison |
| Constraint form vs. index form for UNIQUE | Decision record R-17 | T08 §DB-level UNIQUE vs UNIQUE INDEX |
| Two-bucket destructive classifier | Decision record R-18 | T09 §Prisma classifier |
| `-- DJOGI WARNING:` in UP files | Decision record R-19 | T09 §Warning comment pattern |
| Rename annotation: migration-window-only | Decision record R-20 | T07 §Annotation lifecycle |
| `IndexSpec` partial/functional index support | Decision record R-21 | T08 §Partial and functional indexes |
| `IndexSpec` JSONB `json_path` support | Decision record R-22 | T11 §JSONB and custom types |
| Runner uses dedicated single connection | Decision record R-23 | T04 §deadpool and advisory lock lifecycle |
| `HistoryDiagnostic` taxonomy | Decision record R-25 | T01 §Adopt: Three history-diagnostic states |
| `schema_snapshot.json` `format_version` field | Decision record R-26 | T11 §Snapshot merge conflicts |
| `run_id` is HeerId | Decision record OI-01 | Prisma pattern; HeerId consistency |
| `build.rs` diagnostic-only (re-open) | Decision record Part II, R-12 | T12 §build.rs IDE-churn risk; Phase 7 design §Core Model |
| Apps subsystem — macro, sealed trait, lifecycle | Decision record Part IV | Gap G-22 (new); no prior-art topic citation |
| App FK dependency from type graph | Decision record Part IV | Compile-time inference; no direct analog in surveyed systems |

---

## Appendix B: Rejected Alternatives

### X-01: No snapshot embedding in migration files (cot pattern)

cot embeds schema snapshot structs inside migration files as `#[model(model_type = "migration")]`
annotated code alongside a `const OPERATIONS` list. This couples the execution plan to the
snapshot shape: hand-editing either corrupts future diffs. cot's own implementation hits
`todo!()` at `migration_generator.rs:835` when field type changes cannot be represented in
the embedded struct. Djogi's separate `schema_snapshot.json` side-car avoids all of this
without sacrificing any snapshot functionality. (T12, T11)

### X-02: No current-state-pointer ledger (Alembic pattern)

Alembic's `alembic_version` stores only the current head migration version — not a log. No
timestamp, no checksum, no execution record. "Which migrations ran in last Friday's deploy and
how long did they take?" is unanswerable. Djogi's ledger is a history log: every migration
produces a permanent row. Flyway, Prisma, Liquibase, and Django all chose the log model for
the same reason. (T02, T01)

### X-03: No CRC-32 or SipHash-1-3 checksums

CRC-32 (Flyway) has a 32-bit collision space and is stored as a signed Java `int` that can be
negative — a known awkwardness documented in Flyway's own migration guides. SipHash-1-3
(refinery) hashes `name + version + sql` together, meaning a file rename changes the checksum
even when SQL is byte-for-byte identical. Both are weaker than SHA-256 and have practical design
flaws for the migration use case. (T03)

### X-04: No dedicated lock table (Liquibase pattern)

Liquibase's `DATABASECHANGELOGLOCK` table does not auto-release on process crash. A process
that holds the lock and then dies leaves the table permanently locked until a manual
`releaseLocks` call. Postgres advisory locks release automatically when the TCP connection
tears down — strictly superior for a Postgres-only system. (T04)

### X-05: No filename/version in checksum input (refinery anti-pattern)

Hashing the migration filename or version string into the checksum (refinery's design) makes
the checksum sensitive to metadata that is not operational schema content. Renaming a migration
file to fix a typo in its description should not change its checksum. Only the SQL content (after
normalization) is the correct hash input. (T03, R-05)

---

## Appendix C: Pending Items (Should Be Empty)

All items the research surfaced are now locked. This appendix is retained for symmetry with
typical proposal templates.

Per `docs/research/migrations/2026-04-22/16-decision-record.md` Part VI: every P0 gap,
contradiction, and open item is resolved. All P1 items are locked. All P2 items are explicitly
deferred. All five explicit rejections stand. The six open items (OI-01 through OI-06) are
locked in decision record Part III.

**This appendix is empty. There are no unresolved items.**

If team review produces new open items, they are added here and tracked through a follow-on
decision session before Phase 7 implementation begins.

---

*Decision authority: `docs/research/migrations/2026-04-22/16-decision-record.md`.*
*For system audits of source citations, see topic files in `docs/research/migrations/2026-04-22/topics/`.*
*For project notes on individual surveyed systems, see `docs/research/migrations/2026-04-22/projects/`.*
