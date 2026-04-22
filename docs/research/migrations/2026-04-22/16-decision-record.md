# Decision Record — Migration Research Walkthrough

**Date:** 2026-04-22
**Context:** Captures every decision locked during the user-facing walkthrough of `13-gap-analysis-vs-current-spec.md` and `14-locked-recommendations.md`. This document is the authoritative log of what-was-decided; `migration-proposal.md` (separate artifact) packages these decisions as a team-review proposal against the existing spec.
**Reading order:** This doc supersedes the "open items" and "contradictions" sections of docs 13 and 14. Where this doc and doc 14 disagree on anything locked here, this doc wins.

---

## Part I · Tier-1 Contradictions — Resolved (Housekeeping)

Pure spec cleanup. No design judgment required — the older `docs/spec/migrations.md` §10.1 predates the Phase 7 design doc and contradicts it. The Phase 7 docs win; older doc gets updated.

### C-01 · Runner is Djogi-owned, not `sqlx::migrate`

- **Status:** LOCKED
- **Resolution:** `docs/spec/migrations.md §10.1` updated to state the runner is Djogi-owned, built on `tokio-postgres` + `deadpool-postgres`. No `sqlx::migrate` compatibility layer.
- **Source:** `14-locked-recommendations.md §R-01` (already locked in Phase 7 design/v2 plan).

### C-02 · Ledger table is `djogi_schema_migrations`

- **Status:** LOCKED
- **Resolution:** All references to `_sqlx_migrations` replaced with `djogi_schema_migrations` across Djogi docs and any existing code/fixtures. Consequence of C-01.
- **Source:** `14-locked-recommendations.md §R-02`.

### C-03 · `SchemaDelta` enum expanded

- **Status:** LOCKED
- **Resolution:** `docs/spec/migrations.md §10.6`'s early-sketch enum is superseded. New variants required: `RenameColumn`, `RenameTable`, `CreateEnum`, `AlterEnum`, `DropEnum`, `CreateExtension`, `DropExtension`, `AddUniqueConstraint`, `DropUniqueConstraint`, plus the app-lifecycle variants below.
- **Source:** `14-locked-recommendations.md §R-11`, extended by Part IV here.

---

## Part II · Locked-Decision Re-Opens

### R-12 · `build.rs` is diagnostic-only (re-opens two locked decisions)

- **Status:** LOCKED; two prior locked decisions in `docs/spec/decisions.md` are re-opened and flipped.
- **Resolution:** `build.rs` never writes migration SQL files to `migrations/`. It emits a plain cargo warning on drift. Migration file generation is exclusively via explicit `djogi makemigrations` invocation.
- **Warning format (in `build.rs`):** plain `cargo:warning=djogi: schema drift detected — run \`djogi makemigrations\``. Yellow by cargo default. No spans, no error codes — rustc doesn't expose that surface from `build.rs` on stable, and the simplicity is acceptable here.
- **Colour / rich output:** reserved for `djogi` subcommands (e.g., `djogi migrations status`, `djogi migrations compose --dry-run`) — these have full control over their TTY output via `owo-colors` / `termcolor`.
- **Two-tier rationale:** `build.rs` is the unavoidable every-build surface where terseness matters; `djogi` subcommands are invoked deliberately and can afford rich diagnostics.
- **Supersedes:** `docs/spec/decisions.md` rows "Build drift diagnostic" and "Migration generation." Both need their content updated to match this decision.
- **Source:** `14-locked-recommendations.md §R-12` (was flagged as contradicting locked decisions; now accepted with the re-open).

---

## Part III · Open Items — Locked

### OI-01 · `run_id` column is HeerId

- **Status:** LOCKED
- **Column:** `run_id BIGINT NOT NULL`
- **Generation:** One call to `SELECT generate_id()` at the start of each `djogi migrate` invocation; the returned HeerId is stamped into the `run_id` column of every ledger row written during that run.
- **Rationale:** HeerId is already the canonical ID type everywhere else in Djogi. Time-ordered, 8-byte, zero new dependency, grep-consistent with the rest of the system.
- **Rejected:** UUID v4 (heavier, not time-sortable), ULID (extra dep for no win), custom timestamp strings (redundant when HeerId already solves this).
- **Source:** Prior art at `projects/prisma.md:735` already flagged HeerId as strictly better than Prisma's UUID v4 for their equivalent column.

### OI-02 · `down_checksum` semantics for stub files

- **Status:** LOCKED
- **Rule:** `down_checksum` is NULL only when `_down.sql` genuinely does not exist on disk. If the file exists (even as a comment-only stub), the column stores the SHA-256 of the normalized file bytes.
- **Rationale:** Simpler invariant ("file exists ⇒ checksum non-NULL"). Catches any future hand-edit of a stub as a drift signal. Keeps the NULL case meaningful for legacy imports.
- **Source:** `14-locked-recommendations.md §OI-02`.

### OI-03 · `applied_at` for baseline and fake rows

- **Status:** LOCKED
- **Rule:** `applied_at = now()` at the moment the baseline or fake command ran. Column stays `NOT NULL` for all rows.
- **Semantic carrier:** The `status` column (enum: `applied`, `baseline`, `faked`, `out_of_order`, `rolled_back`, `failed`) distinguishes what kind of row it is. `applied_at` answers "when was this row written," not "when did the migration originally run."
- **Rejected:** Sentinel timestamps (ugly to query), nullable column (query tax on every consumer).
- **Source:** `14-locked-recommendations.md §OI-03`.

### OI-04 · Pending snapshot lives in `target/`, per app

- **Status:** LOCKED
- **Layout:**

| File | Location | Role | Committed? |
|---|---|---|---|
| `target/djogi_models.json` | build artifact | what the Rust source declares | no (gitignored) |
| `target/djogi_pending/<app>.json` | build artifact | generated by `makemigrations`, pre-apply target state | no (gitignored) |
| `migrations/<app>/schema_snapshot.json` | submodule | what is currently applied in the DB | yes |

- **`build.rs` 3-way match logic:**
    - `djogi_models.json == schema_snapshot.json` → silent.
    - Mismatch AND `djogi_pending/<app>.json` matches `djogi_models.json` for that app → `note: migration pending — run 'djogi migrate' to apply`.
    - Mismatch AND no matching pending file → `warning: schema drift detected — run 'djogi makemigrations'`.
- **Lifecycle:** `makemigrations` writes the pending file; `migrate` consumes it (atomic rename into the submodule snapshot, then delete the pending file) on successful completion.
- **Source:** `14-locked-recommendations.md §OI-04` (variant B — separate folder in `target/`).

### OI-05 · Partial-apply tracking is structured, not free-text

- **Status:** LOCKED
- **New ledger columns:**
    - `applied_steps_count INTEGER NOT NULL DEFAULT 0` — incremented after each successful auto-commit.
    - `total_steps INTEGER` — NULL for transactional migrations (single atomic step), set to statement count for non-transactional.
    - `partial_apply_note TEXT` — optional human note, written during repair.
- **Rationale:** Prisma's proven pattern (`projects/prisma.md` / `topics/02-ledger-schema.md`). Queryable, repair-command-integrable, enables `WHERE applied_steps_count >= N` selectors.
- **Rejected:** Free-text `partial_apply_detail TEXT` as the primary signal — retained only as the optional operator-note column.
- **Source:** `14-locked-recommendations.md §OI-05`.

### OI-06 · Snapshot written exactly once per successful `migrate`

- **Status:** LOCKED
- **Rule:** Snapshot update happens at exactly one point: after the final statement of the final migration in the run commits successfully, as an atomic file rename (`tmp → fsync → rename`).
- **Invariant:** Snapshot file never represents partial state. Partial state is representable only via the ledger's `status` + `applied_steps_count` columns.
- **Non-transactional migrations:** statements auto-commit one-by-one, each incrementing `applied_steps_count`. Snapshot is written only when all statements succeed. Failure leaves snapshot untouched and `.migration_failure.json` marker written (R-07).
- **Submodule commit:** one `schema_snapshot.json` change per migration, regardless of statement count. Clean git history.
- **Crash recovery:** If process is killed between ledger COMMIT and snapshot rename → DB fully applied but snapshot stale. `djogi verify` detects; `djogi repair --rebuild-snapshot` regenerates from current ledger + source descriptors. No new machinery beyond existing verify/repair primitives.
- **Rejected:** Per-statement snapshot writes (too many transient states; no benefit over `applied_steps_count`).
- **Source:** `14-locked-recommendations.md §OI-06`.

---

## Part IV · New Subsystem — Apps Architecture

This emerged organically from the `run_id` / app-label questions and became its own design. Not covered in the original docs 13/14 — this is new material locked during the walkthrough.

### Overview

Djogi gains a first-class "app" concept: a centralized, sealed enum of app labels defined once per crate. Apps are organizational metadata; they group migrations into per-app folders and tag ledger rows with an `app_label`. Apps are **opt-in** — users who never invoke `djogi::apps!` retain the current "global, flat" layout.

### The `djogi::apps!` macro

Defined once per crate, typically in `src/apps/mod.rs`:

```rust
djogi::apps! {
    Vehicles,
    Users,
    Orders,
}
```

Expands to:
- One ZST struct per variant (`pub struct Vehicles;`, etc.).
- `impl djogi::App for <Variant>` for each, with `const LABEL: &'static str` auto-derived as the lowercase variant name (override with `#[app(label = "...")]`).
- `impl djogi::__private::SealedApp for <Variant>` — completes the sealed-trait contract.
- A runtime enum `AppRegistry` with a `const ALL: &'static [Self]` slice for introspection.

The `djogi::App` trait is sealed (serde/tokio-style `__private::Sealed` bound). Freeform `impl djogi::App for Foo` is rejected at compile time. A second `djogi::apps!` invocation in the same crate is a compile-time error.

### Opting a model into an app

```rust
#[derive(Model)]
#[model(app = Vehicles)]  // type reference, not string
pub struct Vehicle { ... }
```

Models without `#[model(app = ...)]` default to the global bucket (`app_label = ''`).

### On-disk layout

```
migrations/
├── 0001_initial.sql              ← global (flat, unchanged from spec default)
├── 0002_add_system_cfg.sql
├── vehicles/
│   ├── 0001_initial.sql
│   └── schema_snapshot.json
└── users/
    ├── 0001_initial.sql
    └── schema_snapshot.json
```

### Ledger

`djogi_schema_migrations.app_label TEXT NOT NULL DEFAULT ''` — empty string = global, otherwise the app's `LABEL`. Append-only except for app-rename (which uses a `UPDATE` — the one new exception to the append-only rule, alongside the existing repair command).

### Four lifecycle operations

| Operation | Marker syntax | Migration effect | Ledger effect |
|---|---|---|---|
| **Add** | (none — just add the variant) | None until a model is attached and changes | Normal INSERT on first migration |
| **Rename** | `#[app(renamed_from = "old_label")]` on the new variant | Generate migration in new folder with `UPDATE djogi_schema_migrations SET app_label = 'new' WHERE app_label = 'old'`; `makemigrations` performs the `git mv` of the folder on disk | UPDATE (the one exception) |
| **Remove** | `#[app(tombstone)]` on a variant being retired | Generate destructive migration (drops every table that was in this app); gated by `--allow-destructive` | Normal INSERTs with the retiring `app_label` |
| **Move model** | `#[model(moved_from_app = "old_label")]` on the model | Generate marker migration in new app's folder (SQL no-op); update both source and target `schema_snapshot.json` files at `makemigrations` time; underlying table unchanged | Normal INSERT with marker description |

All four markers are **migration-window-only** — removed after the migration applies. Same lifecycle as the existing `#[field(renamed_from)]` (R-20).

### Compile-time cross-app FK dependency inference

Because `Model::App` is an associated type (concrete ZST at compile time), the differ can resolve `ForeignKey<users::User>` to `<users::User as Model>::App = apps::Users` without runtime lookups. Cross-app dependency edges are computed from the type graph; no Django-style manual `dependencies = [...]` lists required. Topological sort at apply time uses this graph directly.

### Drift detection (new D-codes)

Extends `djogi verify` (R-24) with a three-way app-label check:

- **EnumApps** = `AppRegistry::ALL` ∪ active `renamed_from`/`tombstone` markers
- **SnapshotApps** = union of `registered_apps` across all per-app `schema_snapshot.json` files
- **LedgerApps** = `SELECT DISTINCT app_label FROM djogi_schema_migrations`

| Code | Meaning | Detection | Fires at |
|---|---|---|---|
| **D003** | schema drift (existing, extended) | `djogi_models.json` diverges from snapshot | `build.rs`, `verify` |
| **D004** | app folder drift | filesystem folders differ from `registered_apps` in snapshot | `build.rs`, `verify` |
| **D010** | unknown `app_label` in ledger | `LedgerApps \ (EnumApps ∪ SnapshotApps)` non-empty | `verify` (warn), `migrate` (error, blocks apply) |
| **D011** | model-app mismatch | model's `App` associated type differs from the snapshot-recorded app for that model's table | `verify` |

Override path at migrate time: `--force-apply` (discouraged; writes an `orphan_handled` audit row). Standard reconciliation path: `djogi verify` → `djogi repair` with appropriate subcommand.

### Snapshot extension

`schema_snapshot.json` gains a `registered_apps` field:

```json
{
  "format_version": 1,
  "registered_apps": ["vehicles", "users", "orders"],
  "models": { ... }
}
```

This is the source-of-truth for D003/D004/D010 comparisons.

### Naming locked

- Attribute: **`moved_from_app`** (verbosity preferred for clarity). Parallels `renamed_from` (field-level) and `renamed_from` (app-level) as lifecycle markers.

### Impact on prior recommendations

This subsystem extends or adds to the following recommendations in `14-locked-recommendations.md`:

| R-item | Change |
|---|---|
| **R-04** (ledger DDL) | Add `app_label TEXT NOT NULL DEFAULT ''`, `applied_steps_count INTEGER NOT NULL DEFAULT 0`, `total_steps INTEGER`, `partial_apply_note TEXT` |
| **R-11** (SchemaDelta) | Add `RenameApp { from, to }`, `TombstoneApp { label }`, `MoveModel { model, from_app, to_app }` variants |
| **R-14** / **R-16** | Unchanged at core; pattern now generalizes to app-level markers |
| **R-18** (destructive classifier) | Add `AppRemoval` → `unexecutableSteps` bucket |
| **R-24** (verify) | Extended with the three-way EnumApps/SnapshotApps/LedgerApps check and D003/D004/D010/D011 emission |
| **R-25** (HistoryDiagnostic) | Add `UnknownAppLabel`, `AppFolderDrift`, `ModelAppMismatch` variants |
| **R-26** (snapshot format_version) | Extend snapshot schema with `registered_apps` field |
| **New R-16b** | `#[app(renamed_from)]`, `#[app(tombstone)]`, `#[model(moved_from_app)]` markers spec |
| **New SPEC-M §10.7** | Drift detection semantics and reconciliation workflow |
| **New decision in SPEC-D** | "App lifecycle semantics (add/rename/remove/move-model)" as a locked row |
| **New G-22** in doc 13 | Previously-unnoticed gap: app lifecycle semantics |

---

## Part V · Summary Impact on Docs 13 and 14

### Gaps resolved

Every P0 and P1 gap in `13-gap-analysis-vs-current-spec.md §Part III` is now answered, with the app subsystem adding **G-22** as a fully-designed previously-unnoticed area.

### Contradictions resolved

All three contradictions in `13-gap-analysis-vs-current-spec.md §Part IV` (C-01, C-02, C-03) are resolved in favor of the Phase 7 design docs. Housekeeping updates to `docs/spec/migrations.md` §10.1 / §10.6 required.

### Recommendations — status

- **All P0 recommendations (R-01 through R-16)** in doc 14 are **accepted**, with extensions per Part IV above.
- **R-12** specifically required user re-open of two previously-locked decisions; re-open accepted.
- **All P1 recommendations (R-17 through R-26)** in doc 14 are **accepted**.
- **All P2 recommendations (R-27 through R-31)** remain **deferred** to v0.2+/Phase 7.5 as originally scoped.
- **All five explicit rejections (X-01 through X-05)** stand.
- **All six open items (OI-01 through OI-06)** are **locked** per Part III above.

---

## Part VI · What Happens Next

1. **This document (`16-decision-record.md`) is the authoritative capture.** Future sessions and team members should read this to understand what was decided and why.

2. **Next artifact: `docs/spec/migration-proposal.md`.** A team-review document that positions this locked design as a proposal to compare against the existing `docs/spec/migrations.md` / `docs/spec/decisions.md` / Phase 7 plan. Non-destructive — the existing docs stay untouched until team review concludes.

3. **After team review:** the proposal gets merged into the canonical spec docs, which means:
   - `docs/spec/migrations.md` §10 rewrite
   - New rows in `docs/spec/decisions.md`
   - Phase 7 plan (`docs/superpowers/plans/2026-04-18-phase7-migration-system-v2.md`) amendments

4. **Phase 7 implementation (T1–T8 per the v2 plan)** proceeds against the finalized spec.

---

*End of decision record.*
