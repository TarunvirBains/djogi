# Online-migration parity reference

## Purpose

This document inventories the feature set of [`fatkodima/online_migrations`](https://github.com/fatkodima/online_migrations) — the Ruby/Rails gem that has become the de-facto reference for safe Postgres DDL — and maps each feature to Djogi's current state. It is a **comparison and roadmap reference**, not a design document. Specific API shapes, enum variants, default values, and trait surfaces for any planned item are decided at the start of the relevant phase plan, not here.

The gem matters because:

- It is actively maintained and Postgres-targeted (the same SQL surface Djogi targets).
- Its dangerous-operations checklist is a thorough enumeration of failure modes Djogi must address before Djogi can claim "production-grade migrations."
- It supersets `strong_migrations` (text guidance) by providing **code helpers** that implement the safe alternative when an unsafe migration is detected — a model Djogi's classification gates already partly emulate.

The gem's overall philosophy: **detect → refuse → suggest → provide a helper**, with `safety_assured do ... end` (optionally requiring a written justification) as the documented escape hatch.

## Mapping

Legend:

- ✅ **Shipped** — already in Djogi as of the cited phase.
- 🟡 **Planned (Phase 7.5)** — within scope of the next phase per the README roadmap.
- 🔵 **Deferred** — anchored to a later phase whose theme covers it.
- ⚪ **Not yet anchored** — surfaced by this comparison; phase placement to be decided.

### Classification + dangerous-op gating

| Gem feature | Djogi state |
|---|---|
| Detects unsafe operations (~25 variants) and refuses by default | ✅ Phase 7 — `Classification::{Destructive, Lossy, Unsupported, PkTypeFlip}` with runner-side `--allow-destructive` gate |
| `safety_assured do ... end` opt-out | ✅ Phase 7 — `--allow-destructive` CLI flag (Lossy operations route through `LossyRollbackPolicy` rather than a separate flag) |
| `require_safety_assured_reason = true` (mandatory written justification) | ⚪ Not yet anchored |
| `config.add_check do |method, args| ... end` (custom checks) | ⚪ Not yet anchored |
| `config.disable_check(...)` per-check disable | ⚪ Not yet anchored |
| `OnlineSafe` / staged rollout / offline-only classification dimension orthogonal to destructiveness | 🟡 Phase 7.5 (four variants design-locked in `decisions.md` §79: `OnlineSafe`, `FastLockDestructiveGuarded`, `ExpandContract`, `OfflineOnly`) |
| Per-check custom error messages | ⚪ Not yet anchored |

### Other dangerous-op detectors (gem's Checks list)

The gem's "Checks" section enumerates additional dangerous operations. Each is mapped here even when the parity outcome is "no Djogi action."

| Gem feature | Djogi state |
|---|---|
| `executing SQL directly` (raw `execute(...)` in a migration) | ⚪ Not yet anchored — Djogi's compose flow is descriptor-driven; raw SQL only enters via author-edited `.sql` files. A future detector could refuse author-edited files containing unflagged dangerous statements. |
| `replacing an index` (drop + recreate without overlap) | ⚪ Not yet anchored — falls under Phase 7.5 once concurrent rebuild orchestration lands |
| `adding an exclusion constraint` | ⚪ Not yet anchored |
| `adding a json column` (gem warns to use `jsonb`) | Out-of-scope by design — Djogi only supports `JSONB`; `JSON` columns are not expressible in the descriptor grammar |
| `adding a stored generated column` | ⚪ Not yet anchored |
| `using primary key with short integer type` | ⚪ Not yet anchored — `djogi::primary_key!` allows arbitrary `sql_type` for custom PKs (including `SMALLINT`), so a short-int PK is expressible. The check is a missing detector hook, not a structural impossibility |
| `hash indexes` | ⚪ Not yet anchored — Djogi already emits hash indexes (`IndexType::Hash` in the descriptor, `using = "hash"` in the macro grammar). Missing piece is the gem's hazard detector flagging that hash-index builds cannot run inside a transaction (and historically had crash-recovery issues pre-PG 10) |
| `adding multiple foreign keys` (one migration adding several FKs at once) | ⚪ Not yet anchored — Djogi may emit several `ADD CONSTRAINT` operations from a single descriptor diff; whether to refuse or stagger is a Phase 7.5 question |
| `removing a table with multiple foreign keys` | ⚪ Not yet anchored |
| `mismatched reference column types` (FK column type doesn't match referenced PK type) | ⚪ Not yet anchored — the descriptor's `ForeignKey<T>` carries the target's PK type, so a mismatch is structurally impossible at compose time; verify at projection-time |
| `adding a single table inheritance column` | Out-of-scope by design — Rails STI is a Ruby-class-hierarchy idiom; Djogi has no STI surface |
| `changing the default value of a column` (config-specific check) | ⚪ Not yet anchored |

### Concurrent + non-blocking DDL helpers

| Gem feature | Djogi state |
|---|---|
| `add_index ..., algorithm: :concurrently` / `remove_index ..., algorithm: :concurrently` | ✅ Phase 7 — `IndexSpec.requires_out_of_transaction: bool` + non-transactional segment dispatch |
| `add_column_with_default(...)` 3-step (add NULL column → set default → backfill) | 🟡 Phase 7.5 |
| `update_column_in_batches(..., pause_ms: ...)` chunked + throttled UPDATE | 🟡 Phase 7.5 (backfill orchestration) |
| `initialize_column_type_change` / `backfill_column_for_type_change` / `finalize_column_type_change` / `cleanup_column_type_change` (general column type change) | 🟡 Phase 7.5 (T9 already ships the PK-type variant) |
| `initialize_column_rename` / `finalize_column_rename` via VIEW + alias (zero-downtime rename) | 🟡 Phase 7.5 (Phase 7 ships the direct `RENAME COLUMN` path; the staged path is the 7.5 add) |
| `initialize_table_rename` / `finalize_table_rename` via VIEW | ⚪ Not yet anchored (table rename is rarer than column rename) |
| `add_check_constraint ..., validate: false` then `validate_check_constraint` | 🟡 Phase 7.5 |
| `add_not_null_constraint ..., validate: false` → `validate_not_null_constraint` → `SET NOT NULL` (PG 12+) | 🟡 Phase 7.5 |
| `add_foreign_key ..., validate: false` then `validate_foreign_key` | 🟡 Phase 7.5 |
| `add_unique_constraint ..., using_index: ...` (build CONCURRENTLY index, promote) | 🟡 Phase 7.5 |
| `add_reference_concurrently` (single-call FK + concurrent index) | 🟡 Phase 7.5 |

### Timeouts + retry

| Gem feature | Djogi state |
|---|---|
| `config.statement_timeout = 1.hour` per migration | 🟡 Phase 7.5 |
| `lock_timeout` per migration / per command | 🟡 Phase 7.5 |
| `OnlineMigrations::ExponentialLockRetrier` (retry transaction on lock failure) | 🟡 Phase 7.5 |
| `OnlineMigrations::CommandAwareLockRetrier` (per-method tunables) | 🟡 Phase 7.5 |
| App-side `database.yml` connect_timeout / lock_timeout / statement_timeout recommendations | ⚪ Configuration territory, not Djogi engine |

### Backfill orchestration + side-effect policy

| Gem feature | Djogi state |
|---|---|
| Chunked + throttled backfills | 🟡 Phase 7.5 |
| Side-effect policy for schema-evolution backfills (gem suppresses domain events default-on) | 🟡 Phase 7.5 (named in the implementation plan as a 7e bullet; default behaviour and opt-in shape decided at phase kickoff) |
| Resume-on-restart cursor for long-running backfills | ⚪ Not yet anchored — naturally pairs with the broader background-data-migrations framework once that substrate phase is decided |

### Background-job framework (full async substrate)

None of the existing roadmap phases own a background-job substrate for long-running migrations. These rows are surfaced here as ⚪ so the eventual phase-placement decision is explicit.

| Gem feature | Djogi state |
|---|---|
| Background **data** migrations framework (predefined data migrations, custom enumerators, per-shard parallelism, retry) | ⚪ Not yet anchored |
| Background **schema** migrations framework (long-running CONCURRENTLY index, validate constraint as a tracked job) | ⚪ Not yet anchored |
| Sidekiq integration (gem's default async runner) | ⚪ Not yet anchored — Djogi will pick its own runner abstraction when the substrate phase is decided; this stays pluggable |
| Multiple databases + sharding parallelism for background jobs | ⚪ Not yet anchored — pairs with Phase 11 (Distributed Topology) for the multi-DB story |
| Dashboards / monitoring UI for in-flight migrations | ⚪ Not yet anchored — Phase 9 (Maahi admin) is the natural UI surface once the substrate exists |

### Rollback + audit

| Gem feature | Djogi state |
|---|---|
| Rollback safety (down migrations, checks default-off on down) | ✅ Phase 7 — `rollback` API + ledger-driven down execution |
| Cleanup migrations encode their own reversal | ✅ Phase 7 — descriptor-derived reverse + author-supplied `.down.sql` |
| `config.check_down = true` opt-in for down checks | 🟡 Phase 7.5 |

### Tooling shape

| Gem feature | Djogi state |
|---|---|
| Rails generators (`bin/rails generate online_migrations:install`) | ⚪ Djogi shipping `cargo djogi migrations compose` already covers the equivalent need; no parity action |
| Rake tasks for status / queue management | ✅ Phase 7 — `cargo djogi migrations status` + `attune` cover this |
| Replica-lag awareness on cutover | 🔵 Phase 11 (Distributed Topology) |

## Out-of-scope by design

Some gem features intentionally do not have a Djogi parity goal:

- **Ruby/Rails idioms** — `safety_assured do ... end` block syntax, ActiveRecord-style migration class inheritance. Djogi uses descriptor-driven migrations (the source of truth is the model definition, not a hand-written migration class), so the gem's class-level pattern doesn't translate.
- **Sidekiq lock-in** — the gem ships a default Sidekiq job class; Djogi's eventual background-job framework will be runner-agnostic from day one.
- **`force: true` table refusal** — gem-specific Rails idiom; not relevant to Djogi's compose flow.

## What this comparison did not decide

This document captures the **gap analysis**. It does **not** decide:

- The Phase 7.5 classification enum's payload shape and per-operation default mappings beyond the seed examples in `decisions.md` §79 (variant names themselves ARE locked there).
- API signatures for any planned helper (`add_column_with_default`, `validate_constraint`, `update_in_batches`, etc.).
- Configuration defaults (`statement_timeout`, `lock_timeout`, retry attempts / delays).
- Whether the custom-checks mechanism is a trait, a closure registry, or a config callback.
- Whether `--justify` is a flag, a required ledger field, or both.

All of those land in the Phase 7.5 plan when it kicks off.

## References

- Gem source: <https://github.com/fatkodima/online_migrations>
- README dangerous-ops list: see the gem's "Checks" section
- Djogi roadmap: [`README.md` Status section](../../README.md)
- Djogi migration spec: [`docs/spec/migrations.md`](./migrations.md)
- Djogi decisions log: [`docs/spec/decisions.md`](./decisions.md) (rows §78 + §79 cover the Phase 7 / 7.5 classification handoff)
