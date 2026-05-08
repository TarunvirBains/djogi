# Gap Analysis: Migration Synthesis vs. Current Djogi Spec

**Date:** 2026-04-22
**Input:** Topics 01–12 (migration research synthesis, 2026-04-22)
**References:** `docs/spec/migrations.md`, `docs/spec/decisions.md`, `docs/spec/models.md`,
`docs/spec/adoption-readiness.md`, the local Phase 7 migration-system design notes,
the local Phase 7 migration-system v2 implementation plan
**Output feeds:** `14-locked-recommendations.md`

---

## Executive Summary

Twelve synthesis topics systematically surveyed the migration systems of Flyway, Liquibase, Prisma,
Alembic, Django, SQLx/refinery, SeaORM, cot, and others, then measured Djogi's specs and Phase 7
design against lessons from each domain. This document maps every finding to a concrete status:
**Validated** (the spec already captures the right answer), **Gap** (the area is unresolved or
underspecified), or **Contradiction** (an older spec document says something the Phase 7 design
explicitly overturns).

**Totals: 29 Validated · 21 Gaps · 3 Contradictions**

**Three most important gaps:**

1. Advisory lock key not locked — Djogi must not collide with Prisma's hardcoded `72707369`;
   the exact derivation strategy is absent from every spec document.
2. Ledger DDL is close but incomplete — `source_checksum` disposition, `run_id` presence, `status`
   enum, and surrogate primary key are undecided; the Phase 7 v2 plan has a draft DDL that silently
   drops several research-recommended columns.
3. Checksum algorithm, normalization rules, and version-prefix format are not locked anywhere —
   refinery's SHA-2 mistake (hashing name+version into the content checksum) is a cautionary tale
   that Djogi has not explicitly avoided.

**Biggest contradiction:**

`docs/spec/migrations.md` §10.1 states: "Execution is sqlx's built-in runner — checksummed, tracked
in `_sqlx_migrations`." The Phase 7 design spec explicitly overrules this: "No compatibility
dependency on `sqlx::migrate` should survive into the real Phase 7 system." The Phase 7 v2 plan
proposes a Djogi-owned table named `djogi_schema_migrations`. The old spec text is a live trap for
any reader who does not know which document wins.

---

## Methodology

Each of the twelve research topics was read in full, then cross-referenced against:

- `docs/spec/decisions.md` — 85 locked cross-cutting decisions
- `docs/spec/migrations.md` — migration philosophy and generated SQL contracts
- `docs/spec/models.md` — field and model annotations
- `docs/spec/adoption-readiness.md` — phase gates
- Local Phase 7 migration-system design notes (Phase 7 design)
- Local Phase 7 migration-system v2 implementation plan (Phase 7 v2 implementation plan)

**Status labels:**

| Label | Meaning |
|---|---|
| **Validated** | The spec's design choice is confirmed correct by research; no action needed before `0.1.0` |
| **Gap** | The area is unaddressed or underspecified; a concrete decision is required |
| **Contradiction** | An older spec document says something directly contrary to the Phase 7 design doc or implementation plan |

Items marked **[PRIORITY-1]** must be decided before any Phase 7 coding begins.
Items marked **[PRIORITY-2]** must be decided before Phase 7 is code-complete.
Items marked **[PRIORITY-3]** can be resolved while shipping Phase 7.5 or shortly after.

Sources are cited as abbreviated references:
- `T01`–`T12` = topic files in `docs/research/migrations/2026-04-22/topics/`
- `SPEC-M` = `docs/spec/migrations.md`
- `SPEC-D` = `docs/spec/decisions.md`
- `SPEC-MO` = `docs/spec/models.md`
- `SPEC-AR` = `docs/spec/adoption-readiness.md`
- `P7D` = the local Phase 7 migration-system design notes
- `P7V2` = the local Phase 7 migration-system v2 implementation plan

---

## Part I: Locked Decisions — Status

This section walks every migration-relevant locked decision from `docs/spec/decisions.md` and every
design choice in `docs/spec/migrations.md` and assigns a status.

### I-1 · Default PK type is HeerId (BIGINT)

**Decision (SPEC-D):** "Default PK type — HeerId — `BIGINT DEFAULT generate_id()`, database-native,
time-ordered"

**Status: Validated**

T01 confirms that descriptor-canonical architecture (where the model struct is the single source of
truth) is validated by Prisma and Django. HeerId is orthogonal to migration system design. The
`#[model]` macro injects `id: HeerId` as a real field; the migration system treats it as a `BIGINT
NOT NULL PRIMARY KEY` column. No migration-system decision is blocked by PK type.

**Sources:** T01 §Source of Truth, T12 §Attribute macro comparison.

---

### I-2 · Macro-injected framework fields

**Decision (SPEC-D):** "Framework fields (`id`, `created_at`, `updated_at`) — Macro-injected — real
fields, not written by developer"

**Status: Validated**

The differ must know which columns are always present for every model. Because the macro injects
them deterministically, the differ can treat them as implicitly present in every desired-state
descriptor without the user declaring them. This is consistent with Django's automatic `id`, `auto_now_add`, `auto_now` fields that the migration differ handles transparently.

**Sources:** T11 §Differ correctness, T12 §Attribute macro comparison.

---

### I-3 · Field rename detection via explicit annotation

**Decision (SPEC-D):** "Field rename detection — `#[field(renamed_from = "old_name")]` — differ
treats as rename not drop+add"

**Status: Validated**

T07 validates Djogi's no-heuristic approach explicitly:

> "Prisma's rename heuristic — looking for a field with the same type but a different name — is
> acknowledged to be wrong by Prisma's own team. Django's explicit `RenameField` operation is the
> correct precedent." (T07 §Rename detection comparison)

The explicit annotation avoids silent data loss. The only open sub-question (see Gap G-08) is
whether the annotation must be removed after migration is applied, or whether it can stay as
documentation.

**Sources:** T07 §Validation of Djogi approach, SPEC-D row 55.

---

### I-4 · Build drift diagnostic is a compiler note, not an error

**Decision (SPEC-D):** "Build drift diagnostic — Compiler-style `note` (not error) — migration
generated, build continues, developer reviews"

**Status: Validated**

T11 validates this design:

> "Djogi's decision to emit a compiler-level diagnostic and continue the build is genuinely novel
> in the Rust ecosystem. It surfaces migration work at the natural development checkpoint without
> blocking compilation." (T11 §build.rs approach uniqueness)

T12 confirms no other Rust migration system uses `build.rs` as a migration trigger, which means
Djogi has no prior-art conflicts to navigate.

**Sources:** T11 §build.rs, T12 §Ecosystem contrast.

---

### I-5 · Migration generation automatic via build.rs on drift

**Decision (SPEC-D):** "Migration generation — Automatic via `build.rs` on drift detection —
generates pair, build continues, developer reviews"

**Status: Validated with sub-gap**

The decision is correct. T11 confirms stored-snapshot diffing (Approach D) is O(1) and deterministic
versus Django's O(n) replay. However, T12 surfaces a sub-gap: the distinction between generating
a warning-only diagnostic versus actually writing migration files to disk during `cargo build` has
not been fully specified. See Gap G-17.

**Sources:** T11 §Diff algorithm comparison, T12 §build.rs IDE-churn risk.

---

### I-6 · makemigrations CLI retained as manual trigger

**Decision (SPEC-D):** "makemigrations CLI — Retained as manual trigger for `--dry-run`,
`--allow-destructive`, custom naming"

**Status: Validated**

T09 confirms that `--allow-destructive` flag semantics are the right design for destructive
operations. T11 validates that `makemigrations` as a manual override for `build.rs` automation is
a sound UX pattern (analogous to Django's workflow). P7V2 CLI surface section includes
`cargo djogi makemigrations` with `--dry-run`, `--name <slug>`, `--allow-destructive`,
`--allow-out-of-order`.

**Sources:** T09 §Destructive override, T11 §Operator model, P7V2 §CLI Surface.

---

### I-7 · Schema snapshot updated only on successful migrate

**Decision (SPEC-D):** "Schema snapshot — Updated only on successful `cargo djogi migrate` —
reflects actual DB state, never build state"

**Status: Validated**

This is one of the central design invariants. T01 validates it explicitly:

> "The snapshot must represent what the database believes is applied, not what was planned. Any
> mutation of the snapshot before confirmation of successful apply introduces a split-brain
> condition." (T01 §Snapshot invariant)

P7D also states: "`build.rs` may read the snapshot. It must never mutate it."

The research also identifies a sub-gap: what happens to the snapshot when a partial apply occurs
on a non-transactional migration. See Gap G-06.

**Sources:** T01 §Snapshot invariant, T05 §Non-transactional ledger placement, P7D §Ledger and
Snapshot Model.

---

### I-8 · Migrations folder is a git submodule

**Decision (SPEC-D):** "Migrations folder — Git submodule — pipeline-managed, invisible to
developer day-to-day"

**Status: Validated**

T11 discusses the implications for snapshot merge conflicts (see Gap G-18) but does not challenge
the submodule design as a workflow choice. The submodule approach delegates version tracking of
migration files to the CI/CD pipeline cleanly.

**Sources:** T11 §Snapshot format, SPEC-D row 60.

---

### I-9 · Down files always generated as a pair

**Decision (SPEC-D):** "Migration down files — Always generated as a pair; data loss on destructive
rollback documented in file"

**Status: Validated**

P7D addresses the matrix's forward-only recommendation explicitly and rejects it:

> "The migration matrix recommends forward-only as the default. After scrutiny, Djogi should
> **not** adopt that recommendation. ... Djogi keeps paired `up` / `down` files as a hard
> contract." (P7D §Reversibility Decision)

T09 validates that down files must carry explicit warning headers for destructive operations:
lossy rollbacks should be labeled clearly in the generated SQL, not discovered at rollback time.

**Sources:** P7D §Reversibility Decision, T09 §Down file warning headers.

---

### I-10 · Database target is Postgres only (permanent)

**Decision (SPEC-D):** "Database target — Postgres only — permanent decision, not a limitation;
enables JSONB, HeeRanjId, advisory locks, transactional DDL, `RETURNING`"

**Status: Validated**

T04 relies on Postgres advisory locking semantics throughout. T05 references Postgres-specific DDL
transaction semantics (`CREATE INDEX CONCURRENTLY`, Postgres 18 transactional DDL improvements).
T08 uses Postgres constraint grammar (`ADD CONSTRAINT ... UNIQUE`). T10 references `LOCK NOWAIT`
and Postgres-native online DDL features. T12 confirms the Postgres-only stance is an advantage
in the Rust ecosystem — no other system commits to Postgres 18+ exclusively.

**Sources:** T04, T05, T08, T10, T12 throughout.

---

### I-11 · Postgres version floor is Postgres 18

**Decision (SPEC-D):** "Postgres version floor — Postgres 18 — no support for older versions"

**Status: Validated**

T05 confirms that `CREATE INDEX CONCURRENTLY` non-transactional behavior is unchanged from
earlier versions but that Postgres 18 offers specific DDL and protocol improvements Djogi can use
freely. T10 notes that several online-safe techniques (e.g. `VALIDATE CONSTRAINT`) are available
well before Postgres 18, meaning the floor does not restrict Phase 7.5 design. No topic finds a
reason to lower or raise the floor.

**Sources:** T05 §Postgres DDL semantics, T10 §Online-safe constraints.

---

### I-12 · No regex anywhere in Djogi

**Decision (SPEC-D):** "Regex anywhere in djogi — Prohibited — no exceptions."

**Status: Validated**

T05 notes that Flyway's non-transactional detection uses keyword scanning (which Djogi explicitly
rejects): "Plain-SQL validation is useful as a guardrail, but string scanning must not be the
primary design." (P7D §Transaction Boundary Decision). Djogi's approach of deriving
transactional/non-transactional status from structured operation kinds and descriptor metadata is
a direct consequence of the regex prohibition — and is the *correct* design by independent
analysis. No topic requires or suggests a regex.

**Sources:** T05 §Keyword-list rejection, P7D §Transaction Boundary Decision, SPEC-D row 79.

---

### I-13 · FK cascade default is RESTRICT

**Decision (SPEC-D):** "FK cascade default — `RESTRICT` — safest Postgres default; overridden
per-field with `#[field(on_delete = "...")]`"

**Status: Validated**

T11 confirms that FK constraint changes (including `on_delete` policy changes) must generate a
real constraint diff, not just emit a no-op. The `RESTRICT` default means most FKs are safe to
add in a transactional segment; a cascade policy change requires drop+recreate of the constraint.
No topic challenges the default.

**Sources:** T11 §Differ correctness edge cases, SPEC-M §10.6 SchemaDelta.

---

### I-14 · Composite unique constraints and indexes are 0.1.0 scope; composite PKs are not

**Decision (SPEC-M §10.1, P7D §Composite Key Boundary):** "Composite unique constraints and
composite indexes are part of the migration surface; composite primary keys are not part of the
`0.1.0` contract"

**Status: Validated**

T08 provides the deepest coverage of this topic. The research validates the scope decision:

> "Composite unique constraints are used far more often than composite primary keys in
> production Postgres applications. Supporting the former without the latter is a principled
> boundary." (T08 §Scope analysis)

T08 also surfaces important sub-gaps in the attribute syntax and naming convention. See Gaps G-09
and G-10.

**Sources:** T08 §Scope analysis, P7D §Composite Key Boundary, SPEC-M §10.1.

---

### I-15 · Raw escape hatch always available

**Decision (SPEC-D):** "Raw escape hatch — Always available on `DjogiContext` — `raw_query<T>`,
`raw_fetch_one<T>`, `raw_scalar<T>`, `raw_execute`"

**Status: Validated**

T10 confirms that no surveyed system fully automates online-safe migrations; Djogi's raw escape
hatch is part of the answer for operations the planner cannot safely automate. T12 confirms this
pattern is consistent with the "never hide SQLx" principle.

**Sources:** T10 §Ecosystem gap, T12 §Raw escape hatch.

---

### I-16 · Dirty tracking off by default, opt-in

**Decision (SPEC-D):** "Dirty tracking — Off by default; opt-in globally via `Djogi.toml` or
per-model via `#[model(dirty_tracking)]`"

**Status: Validated**

No topic challenges this. The migration system is independent of dirty tracking — the differ
compares descriptors against the snapshot, not runtime model state.

---

### I-17 · CLI interface is `cargo djogi` subcommand

**Decision (SPEC-D):** "CLI interface — `cargo djogi` subcommand — installed via `cargo install
djogi-cli`, idiomatic Rust toolchain"

**Status: Validated**

P7V2 §CLI Surface enumerates all migration-related subcommands within this pattern. T12 confirms
the approach is consistent with the Rust ecosystem.

---

### I-18 · `Jsonb<T>` field type with unknown field preservation

**Decision (SPEC-D):** "`Jsonb<T>` field type — `JSONB` column with typed schema, serde
deserialization, validator validation, nested schema support"

**Status: Validated with sub-gap**

T11 notes that `Jsonb<T>` index metadata (JSONB path indexes) must be handled in the descriptor
diff — a requirement no other Rust migration system handles. The basic `JSONB` column creation is
straightforward; the index metadata for JSONB subfields is a Djogi-specific gap. See Gap G-15.

**Sources:** T11 §JSONB and custom types, T12 §Djogi-specific types.

---

### I-19 · Migration generation as SQL (not Rust code)

**Decision (P7D §Primary Artifact Choice):** "Djogi generates plain SQL migration files as the
default artifact"

**Status: Validated**

T12 traces the SeaORM / cot Rust-code-migration path and shows the failure mode: when field type
changes are encountered, cot hits `todo!()` at `migration_generator.rs:835`. Generated Rust
migration code is also harder to review in code review and harder to debug during incidents. SQL
remains the right primary artifact.

**Sources:** T12 §cot limitations, P7D §Primary Artifact Choice.

---

### I-20 · Djogi owns the migration runner (no sqlx::migrate)

**Decision (P7D §Runner Ownership):** "No compatibility dependency on `sqlx::migrate` should
survive into the real Phase 7 system."

**Status: Validated**

T03 confirms refinery's SipHash-1-3 approach (which is coupled to sqlx's file-naming conventions)
breaks on migration file renames. T04 confirms `sqlx::migrate` does not use advisory locks. T05
confirms that `sqlx::migrate` runs every DDL statement in a transaction without awareness of
non-transactional DDL requirements. All three failures are resolved by Djogi owning the runner.

**Sources:** T03 §Refinery checksum flaw, T04 §Advisory lock gap in sqlx, T05 §Transaction
boundary design, P7D §Runner Ownership.

---

### I-21 · tokio + tokio-postgres + deadpool-postgres stack

**Decision (P7V2 §Tech Stack):** "Rust stable · `tokio-postgres 0.7` · `deadpool-postgres 0.14`"

**Status: Validated**

T12 validates this choice. T04 identifies a nuance around `deadpool-postgres` and advisory lock
behavior after process crash — see Gap G-04.

**Sources:** T12 §Runtime and driver validation, T04 §Advisory lock release on crash.

---

### I-22 · Advisory locking must be used for concurrent migrator safety

**Decision (P7D §Runner, P7V2 §T5):** Advisory lock acquisition/release is part of the runner's
responsibilities.

**Status: Validated**

T04 provides the full analysis. Flyway, Prisma, and Liquibase all use advisory locking. No
surveyed Rust system uses advisory locking today, making this a Djogi differentiator. The decision
to adopt it is correct.

**Sources:** T04 throughout, P7D §Runner Ownership.

---

### I-23 · Repair must be a first-class workflow

**Decision (P7D §Failure and Repair Model):** "Repair must be a first-class workflow."

**Status: Validated**

T03 provides detailed analysis of Flyway's three repair operations (checksum repair, description
update, delete failed rows) and Prisma's state machine approach (refuses to touch successful rows).
The hybrid of both is the recommended design.

**Sources:** T03 §Repair workflows, P7D §Failure and Repair Model.

---

### I-24 · Out-of-order policy is environment-sensitive

**Decision (P7D §Out-of-Order Policy):** "local/dev: allow by default, but record and warn
loudly; CI/prod: reject by default; explicit override available in all environments"

**Status: Validated**

T06 validates this tiered approach. Global rejection is too painful for branch-based development;
global allowance is too risky for production. The tiered policy with explicit ledger recording is
the correct design.

**Sources:** T06 §Out-of-order policy analysis, P7D §Out-of-Order Policy.

---

### I-25 · Baseline and fake are first-class flows

**Decision (P7D §Baseline Adoption):** "Baseline adoption is mandatory. Djogi should ship:
`cargo djogi migrate baseline <version>` / `cargo djogi migrate --fake <version>`"

**Status: Validated**

T06 distinguishes three primitives (baseline, stamp/fake, and force-mark) and confirms Djogi needs
at minimum the first two. T03 confirms that fake/baseline flows must preserve future diff
determinism.

**Sources:** T06 §Baseline semantics, T03 §Fake semantics, P7D §Baseline Adoption.

---

### I-26 · Partial non-transactional apply must be explicitly tracked

**Decision (P7D §Failure and Repair Model):** "Partial non-transactional apply must be explicitly
recorded in the ledger and block normal migration progression until repaired."

**Status: Validated**

T05 confirms that the absence of an atomic transaction boundary for `CREATE INDEX CONCURRENTLY`
and similar operations means failure partway through cannot be auto-rolled back. The ledger must
record this state. The Phase 7 v2 DDL includes `partial_apply_state` and `partial_apply_detail`
columns for this purpose.

**Sources:** T05 §Non-transactional failure handling, P7D §Failure and Repair Model.

---

### I-27 · Destructions require explicit classification

**Decision (SPEC-M §10.2):** "Destructive operations (DROP COLUMN, DROP TABLE) emit a warning
and require `--allow-destructive` to proceed"

**Status: Validated with sub-gap**

T09 confirms the two-bucket destructive classification (blocking `unexecutableSteps` versus
non-blocking `warnings`) is the right design. The research recommends Djogi be stricter than
Prisma: DROP COLUMN and DROP TABLE should be `unexecutableSteps` (require explicit flag), not mere
warnings. See Gap G-11 for the full operation-to-bucket mapping that has not yet been written into
spec.

**Sources:** T09 §Destructive classification comparison, SPEC-M §10.2.

---

### I-28 · Data migrations are deferred (SQL companion files only in Phase 7)

**Decision (P7D §Data Migration Decision):** "historical Rust data migrations are deferred;
schema runner correctness comes first"

**Status: Validated**

T01 explicitly defers this. T12 notes cot's approach to RunSQL-equivalent companions. Phase 7
supports explicit SQL data migration companions while deferring Django-style `HistoricContext<'a>`
Rust data migrations.

**Sources:** T01 §Data migration scope, P7D §Data Migration Decision.

---

### I-29 · Phase 7.5 is pre-0.1.0 scope (online-safe staged migrations)

**Decision (SPEC-AR §Phase 7.5):** "Online-safe staged live migrations safe at Phase 7.5" and
P7D §Required Before 0.1.0: "Phase 7.5 online-safety classification + resumable backfill /
cutover / finalize operator flows"

**Status: Validated**

T10 confirms that every surveyed migration system punts on online-safe automation. Djogi shipping
Phase 7.5 before `0.1.0` would be a genuine differentiator. T10 identifies five specific staged
patterns that Phase 7.5 must support (matched in P7D §Live Migration Patterns Required Before
0.1.0).

**Sources:** T10 §Ecosystem gap, SPEC-AR §Phase 7.5, P7D §Required Before 0.1.0.

---

## Part II: Phase 7 Design — Status

This section reviews Phase 7-specific design decisions that appear in P7D and P7V2 but have not
yet been locked into `docs/spec/`.

### II-1 · Three-truth model (desired, applied, operational)

**P7D statement:** "Djogi treats migrations as a system with three distinct truths: 1. Desired
schema — derived from registered `#[model]` descriptors; 2. Applied schema model — stored in
`migrations/schema_snapshot.json`; 3. Operational history — stored in migration files plus a
migration ledger table in the target database."

**Status: Validated**

T01 independently derives the same three-truth model:

> "A migration system needs three distinct representations of truth to operate safely:
> the intended schema, the applied schema, and the execution history. Conflating any two of these
> creates a split-brain condition that becomes visible only at the worst possible moment." (T01
> §Three-truth analysis)

**Sources:** T01 §Three-truth analysis, P7D §Core Model.

---

### II-2 · Snapshot must never be mutated by build.rs

**P7D statement:** "`build.rs` may read the snapshot. It must never mutate it."

**Status: Validated**

T11 §Diff algorithm confirms that any write to the snapshot outside of the runner creates a
correctness hazard. The warning-only diagnostic (I-4) is the direct consequence of this.

**Sources:** T11 §Snapshot mutations, P7D §Snapshot.

---

### II-3 · Differ reads snapshot, not live catalog

**P7D statement:** "Djogi does not plan by diffing directly against the live database catalog on
every build or CLI invocation. The planner diffs: desired schema from descriptors / applied schema
from `schema_snapshot.json`."

**Status: Validated**

T11 §Diff algorithm compares four approaches and confirms stored-snapshot (Approach D) is O(1),
reproducible, and CI-safe without a live database connection. Django's O(n) replay is the main
alternative; it is known to be slow at scale.

**Sources:** T11 §Approach D validation, P7D §Core Model.

---

### II-4 · Snapshot shape must include enums, extensions, partitions

**P7V2 statement:** "The format must be deterministic for review diffs" and includes "enum types
and variants; required extensions; partition metadata where relevant."

**Status: Validated with sub-gap**

T08 confirms JSONB subfield index metadata must also be in the snapshot. The Phase 7 v2 snapshot
shape description does not list this explicitly. See Gap G-15.

**Sources:** T08 §JSONB index handling, T11 §Snapshot completeness, P7V2 §Snapshot shape.

---

### II-5 · Transactional/non-transactional segment planning from operations, not text scanning

**P7D statement:** "This decision should be derived primarily from structured migration operations
/ descriptor-owned metadata such as `requires_out_of_transaction` / extension/index semantics
already known to Djogi. Plain-SQL validation is useful as a guardrail, but string scanning must
not be the primary design."

**Status: Validated**

T05 validates this explicitly. Flyway's keyword-list approach is brittle; Djogi's structured
approach that reads operation type metadata is correct.

**Sources:** T05 §Transaction boundary planning, P7D §Transaction Boundary Decision.

---

### II-6 · `-- djogi:no-transaction` header directive for manual override

**Mentioned in:** T05 §Non-transactional ledger placement.

**Status: Validated with sub-gap**

The directive is referenced but not formally specified in SPEC-M or P7V2. Its exact behavior
(must appear on line 1, takes precedence over structured detection, affects segment planning) is
not written into any spec. See Gap G-05.

**Sources:** T05 §Directive syntax.

---

### II-7 · Rollback ordered by installed_rank (not version number)

**P7D §Reversibility Decision** states rollback exists but does not specify ordering.

**Status: Validated with sub-gap**

T06 identifies that rollback must operate on temporal application order (installed_rank), not
on version string order. If migration 0009 is applied before 0008 in a branched environment, the
rollback must undo 0009 before 0008. This is not specified anywhere in the Djogi spec. See Gap
G-07.

**Sources:** T06 §Rollback ordering, P7D §Reversibility Decision.

---

### II-8 · `djogi verify` command to bridge snapshot-vs-live gap

**Mentioned in:** T11 §Shadow DB alternative.

**Status: Gap**

T11 identifies that Prisma uses a shadow database to verify planner output against a live
database. Djogi rejects the shadow database approach (requires `CREATE DATABASE` permission;
complex setup). T11 recommends a `djogi verify` command that compares the snapshot against the
actual live catalog without a shadow database. This command is not mentioned in P7V2 CLI Surface.
See Gap G-19.

**Sources:** T11 §Shadow DB vs djogi verify.

---

## Part III: Gaps — Underspecified Areas

Each item below identifies an area where the research found Djogi's design to be incomplete,
missing a critical decision, or relying on an implicit assumption that should be made explicit
before Phase 7 implementation begins.

### G-01 · Advisory lock key derivation [PRIORITY-1]

**Research finding (T04):** Prisma uses `SELECT pg_advisory_lock(72707369)` — a hardcoded magic
constant. Flyway uses `pg_try_advisory_lock(LOCK_MAGIC_NUM + hashCode(table))` over a range.
Djogi must not collide with either. T04 suggests `x'DJOGMIGR'::bigint` as a candidate to produce
a distinct Bigint key.

**Current spec state:** Nowhere in SPEC-D, SPEC-M, P7D, or P7V2 is the Djogi advisory lock key
value specified.

**Why it matters:** A collision with Prisma's constant would allow a `prisma migrate` process to
unintentionally block or be blocked by `cargo djogi migrate` on shared infrastructure. This is a
production correctness issue, not just a performance concern.

**Action required:** Lock the exact advisory lock key derivation strategy (chosen constant or
derivation function) in the Phase 7 spec before the runner is implemented.

**Sources:** T04 §Lock key collision risk, P7D §Runner Ownership.

---

### G-02 · Checksum algorithm, normalization, and version prefix [PRIORITY-1]

**Research finding (T03):** Three decisions must be made together:

1. Hash function: SHA-256 (not refinery's SipHash-1-3, which is non-cryptographic and
   migration-rename-hostile because it hashes name+version into content checksum).
2. Normalization: strip BOM, normalize all line endings to `\n`, no trailing whitespace or comment
   stripping.
3. Version prefix: adopt Liquibase's `V:hex` format (`V1:abcdef...64hexchars`), where the leading
   `V1:` prefix allows future algorithm rotation without breaking stored checksums.

**Current spec state:** SPEC-M §10.1 mentions "checksummed" without specifying the algorithm.
P7V2 ledger DDL has `checksum_up TEXT NOT NULL` and `checksum_down TEXT NOT NULL` without format
specification.

**Why it matters:** Getting this wrong means migration repair workflows cannot reliably detect
legitimate vs. accidental file edits. Refinery's design (hashing filename + version + SQL into one
value) means that renaming a file changes the checksum even if the SQL is identical.

**Action required:** Document the exact checksum algorithm, normalization rules, and storage
format before ledger implementation.

**Sources:** T03 §Checksum design, T03 §Refinery anti-pattern.

---

### G-03 · Ledger DDL is incomplete [PRIORITY-1]

**Research finding (T02):** The Phase 7 v2 plan's current ledger DDL is:

```sql
CREATE TABLE IF NOT EXISTS djogi_schema_migrations (
    version                  TEXT PRIMARY KEY,
    description              TEXT NOT NULL,
    checksum_up              TEXT NOT NULL,
    checksum_down            TEXT NOT NULL,
    applied_at               TIMESTAMPTZ NOT NULL,
    execution_mode           TEXT NOT NULL,
    out_of_order             BOOLEAN NOT NULL DEFAULT FALSE,
    partial_apply_state      TEXT,
    partial_apply_detail     TEXT,
    snapshot_version         TEXT NOT NULL
);
```

T02 recommends the following additions and changes, not yet incorporated:

- `status TEXT NOT NULL DEFAULT 'applied' CHECK (status IN ('pending', 'applied', 'failed',
  'rolled_back'))` — Prisma's pre-write row pattern eliminates the crash window where a migration
  runs but the ledger row is never written.
- `run_id TEXT` — Liquibase's DEPLOYMENT_ID concept; groups all migrations applied in one
  `cargo djogi migrate` invocation for rollback, audit, and diagnostics.
- `applied_by TEXT NOT NULL DEFAULT current_user` — Flyway's `installed_by` equivalent; tracks
  who ran the migration.
- Surrogate `BIGINT` primary key alongside natural `TEXT UNIQUE` version — enables stable
  `installed_rank` ordering independent of version string sort.

T02 recommends dropping `source_checksum` (build-time artifact, not runtime concern). The current
draft does not have it, but T02 explicitly documents why it should not be added if proposed.

**Current spec state:** P7V2 §Ledger shape has the current DDL. SPEC-M does not enumerate ledger
columns at all.

**Why it matters:** Each missing column closes a specific failure window. `status = 'pending'` is
the cleanest solution to the crash-window problem. `run_id` enables deployment-level rollback.
`applied_by` is baseline audit hygiene.

**Action required:** Finalize ledger DDL with all four additions before T1/T2 implementation.

**Sources:** T02 §Ledger DDL recommendations, T02 §Status column, T02 §run_id.

---

### G-04 · deadpool-postgres advisory lock release behavior [PRIORITY-2]

**Research finding (T04):** Session-scoped advisory locks are released when the connection closes.
`deadpool-postgres` pools connections, which means:

1. After a process crash, the advisory lock may persist until the pool connection is closed by
   the Postgres server's TCP timeout.
2. Keep-alive settings on the pool can extend the window during which a crashed migrator holds
   the lock.

This affects the time window between a failed deploy and the ability to run the next migration.

**Current spec state:** Not addressed in any spec or plan document.

**Why it matters:** If a deployment crashes mid-migration and the connection stays pooled for
several minutes, the next deploy cannot acquire the advisory lock and will fail for that duration.

**Action required:** Specify the advisory lock acquisition mode (`pg_try_advisory_lock` vs.
`pg_advisory_lock` with timeout) and the deadpool connection lifecycle policy for the migration
runner. The runner should use a dedicated single connection (not a pool) for the duration of the
migration, released explicitly on success, failure, or process signal.

**Sources:** T04 §deadpool and advisory lock lifecycle, T04 §Session vs transaction scope.

---

### G-05 · `-- djogi:no-transaction` directive specification [PRIORITY-2]

**Research finding (T05):** The `-- djogi:no-transaction` header directive is mentioned in the
research but has no formal specification in any Djogi document. Key questions:

- Must it appear on the first non-blank line, or can it appear anywhere in the header block?
- Does it override or supplement structured operation detection?
- What is the runner's behavior when the directive is present — does it wrap the entire file as
  one non-transactional segment, or does segment planning still apply within the file?
- Can the directive appear in a down file independently of the up file?

**Current spec state:** SPEC-M §10.4 shows file headers but does not include the directive.
P7V2 §Generated file headers shows `-- Execution-Mode: transactional` but does not show the
no-transaction directive syntax.

**Why it matters:** Operators writing hand-edited migration files need this directive for advanced
cases (e.g., `CREATE EXTENSION` which may or may not be transactional depending on the extension).
The runner must parse it correctly.

**Action required:** Specify directive syntax, parsing rules, and behavior in SPEC-M or P7D.

**Sources:** T05 §Directive design.

---

### G-06 · Snapshot invariant under partial non-transactional failure [PRIORITY-2]

**Research finding (T01, T05):** If a non-transactional migration segment fails partway through,
the snapshot cannot be advanced (the migration is not complete) but the snapshot also cannot
accurately reflect an intermediate state it was not designed to represent.

T01 recommends a `migration_failure.json` or similar marker file that records:
- the failed version
- the segment that failed
- the expected next snapshot version

The runner would refuse to plan or apply further until the marker is resolved by `repair`.

**Current spec state:** P7V2 ledger DDL includes `partial_apply_state` and `partial_apply_detail`,
which records the ledger side. The snapshot side is not addressed.

**Why it matters:** Without the marker, there is a risk that the snapshot and ledger agree that
something failed, but the runner does not prevent a second `migrate` attempt from planning against
stale snapshot state.

**Action required:** Specify the snapshot-side invariant for partial failure: what file or flag
prevents planning from proceeding, and how `repair` clears it.

**Sources:** T01 §Snapshot invariant, T05 §Partial apply failure.

---

### G-07 · Rollback ordering under out-of-order apply [PRIORITY-2]

**Research finding (T06):** When migrations are applied out of order in a branched development
environment, rollback must operate on `installed_rank` (temporal application order), not on
version number order. If the ledger shows:

```
applied_at  version
10:00       0009     ← applied first on this branch
10:05       0008     ← applied second (out of order)
```

Then rollback must undo 0008 first (most recently applied), then 0009 — not undo 0009 first
because its version number is higher.

**Current spec state:** P7D §Reversibility Decision states rollback is supported but does not
specify the ordering semantics.

**Why it matters:** Wrong rollback ordering can create schema states that violate FK constraints
or produce incorrect snapshots.

**Action required:** Lock rollback ordering as `installed_rank` (temporal) order in the spec.
The `status` column's `rolled_back` value must be set in reverse-temporal order.

**Sources:** T06 §Rollback ordering semantics.

---

### G-08 · Lifecycle of `#[field(renamed_from)]` after migration [PRIORITY-3]

**Research finding (T07):** After a rename migration is generated and applied, should the
`#[field(renamed_from = "old_name")]` annotation be removed from the source code?

- If it stays: the differ must detect that the rename is already reflected in the snapshot and
  emit no further migration. This requires the differ to track rename history.
- If it is removed: the annotation is self-documenting only during the migration window.

T07 raises this as an open question. Django's `RenameField` operation is removed from the model
after the migration is applied (the migration file records it permanently). Djogi's annotation
approach is different enough that the lifecycle needs explicit design.

**Current spec state:** SPEC-MO and SPEC-D do not address what happens after the rename migration
is applied.

**Action required:** Specify whether the annotation is a permanent documentation aid (requiring
differ logic to detect already-applied renames) or a migration-window-only marker (to be removed
once the migration is applied).

**Sources:** T07 §Annotation lifecycle, SPEC-MO §Field annotations.

---

### G-09 · Composite constraint attribute syntax [PRIORITY-2]

**Research finding (T08):** Multi-column unique constraints and multi-column indexes require
model-level (not field-level) attribute syntax because they span multiple fields. The current
SPEC-MO only documents single-field annotations:

```rust
#[field(unique)]
#[field(index)]
```

For composite constraints, T08 finds no agreed syntax in Djogi's spec. Candidate approaches from
the ecosystem:

```rust
// Model-level attribute (Diesel-style)
#[model(unique(fields = ["col_a", "col_b"]))]

// Separate top-level attribute (SQLAlchemy-style)
#[unique_constraint(col_a, col_b)]

// Derive-adjacent attribute block
#[derive(Model)]
#[indexes(unique(col_a, col_b), index(col_c, col_d))]
```

**Current spec state:** SPEC-M §10.1 states composite unique constraints are in scope but gives
no attribute syntax. SPEC-MO lists only single-field annotations.

**Why it matters:** The macro must be designed to accept this syntax before the differ can produce
composite constraint descriptors.

**Action required:** Lock the composite constraint/index attribute syntax in SPEC-MO.

**Sources:** T08 §Composite attribute syntax, P7D §Composite Key Boundary.

---

### G-10 · Composite constraint and index naming convention [PRIORITY-2]

**Research finding (T08):** Generated names for composite constraints and indexes must be:

1. Deterministic (same inputs always produce the same name)
2. Stable (reordering unrelated schema objects does not change generated names)
3. Truncated safely for Postgres's 63-byte identifier limit

T08 recommends:
- Unique constraint: `<table>_<col1>_<col2>_key`
- Non-unique index: `<table>_<col1>_<col2>_idx`
- On truncation: SHA-256 of the full name, take first 8 hex characters as suffix, truncate
  table+col portion to fit within 63 bytes.

**Current spec state:** SPEC-M shows an example index name (`idx_vehicles_horsepower`) but does
not specify the algorithm for composite names or truncation.

**Why it matters:** Non-deterministic names cause unnecessary migration churn (differ thinks
the index was dropped and re-created).

**Action required:** Lock naming conventions and truncation algorithm in SPEC-M or a new naming
spec. Cross-reference with the P7V2 `naming.rs` module.

**Sources:** T08 §Naming convention, T08 §Truncation algorithm.

---

### G-11 · Destructive classifier operation-to-bucket mapping [PRIORITY-2]

**Research finding (T09):** T09 provides a recommended operation-to-bucket table that is more
complete than SPEC-M's list. The key additions and differences:

| Operation | Prisma bucket | Recommended Djogi bucket |
|---|---|---|
| `DROP TABLE` | `warnings` | `unexecutableSteps` (requires `--allow-destructive`) |
| `DROP COLUMN` | `warnings` | `unexecutableSteps` |
| `ALTER COLUMN` (type narrowing) | `warnings` | `warnings` (allow with explicit warning in SQL) |
| `DROP INDEX` | Not classified | `warnings` |
| `DROP FOREIGN KEY` | Not classified | `warnings` |
| `ALTER COLUMN` (nullable → NOT NULL) | `unexecutableSteps` | `unexecutableSteps` |
| `RENAME COLUMN` | Not classified | `warnings` (if rename annotation present, else `unexecutableSteps`) |
| Enum value deletion | `unexecutableSteps` | `unexecutableSteps` |
| Enum value reorder | `warnings` | `unexecutableSteps` |

**Current spec state:** SPEC-M §10.2 says destructive operations "emit a warning and require
`--allow-destructive`" but does not enumerate which operations are destructive vs. which are
merely lossy.

**Why it matters:** The bucket determines whether the migration is blocked or merely warned. The
wrong bucket causes either too much friction (blocking safe operations) or too little (silently
allowing dangerous ones).

**Action required:** Lock the operation-to-bucket table in SPEC-M or a dedicated classifier spec.

**Sources:** T09 §Operation-to-bucket table, SPEC-M §10.2.

---

### G-12 · Warnings embedded in generated SQL as comments [PRIORITY-3]

**Research finding (T09):** Prisma embeds destructive/lossy warnings as SQL comments in the
generated file. This creates a paper trail that is visible in code review even without running
`plan`. T09 recommends Djogi adopt this pattern.

**Current spec state:** SPEC-M §10.4 shows a DOWN file comment:
```sql
-- WARNING: dropping a column is irreversible — data is not recoverable on rollback
```
This is present for down files but not for up files that contain destructive operations.

**Why it matters:** During code review, reviewers see the up file. The warning in the down file
only appears if the reviewer opens both files.

**Action required:** Add destructive/lossy classification comments to UP files when the operation
has data-loss implications (e.g., `ALTER COLUMN` type narrowing).

**Sources:** T09 §Warning comment pattern, SPEC-M §10.4.

---

### G-13 · `#[model(renamed_from = "old_table")]` not in spec [PRIORITY-2]

**Research finding (T07):** `#[field(renamed_from = "old_name")]` is in SPEC-D and SPEC-MO for
field renames. But T07 notes that table renames — which generate `ALTER TABLE old_name RENAME TO
new_name` — require an analogous model-level annotation. The Phase 7 design spec mentions this:

> "`#[model(renamed_from = "...")]`" appears in P7D §Rename Decision and P7V2 §Canonical Scope.

**Current spec state:** SPEC-D row 55 mentions only `#[field(renamed_from = "old_name")]`.
`#[model(renamed_from = "...")]` is referenced in Phase 7 documents but not locked in the base
spec.

**Action required:** Add `#[model(renamed_from = "old_table")]` to SPEC-MO and SPEC-D as a locked
annotation, mirroring the field-level annotation decision.

**Sources:** T07 §Table rename handling, P7D §Rename Decision, P7V2 §Canonical Scope.

---

### G-14 · Partial/functional index descriptor representation [PRIORITY-2]

**Research finding (T08):** Partial indexes (`WHERE` clause) and functional indexes
(`LOWER(email)`) are important Postgres features that no surveyed Rust migration system handles.
T08 notes this as a Djogi opportunity to differentiate.

For the migration system, the descriptor must be able to represent:
- the index predicate (for partial indexes)
- the expression (for functional indexes)

These cannot be expressed as simple column-reference lists.

**Current spec state:** SPEC-M `IndexDef` and P7V2 snapshot shape do not include predicate or
expression fields.

**Action required:** Decide whether partial/functional index support is Phase 7 or post-0.1.0.
If Phase 7, specify the `IndexDef` extension. If deferred, document the deferral explicitly.

**Sources:** T08 §Partial and functional indexes.

---

### G-15 · JSONB subfield index descriptor representation [PRIORITY-2]

**Research finding (T11, T12):** `Jsonb<T>` fields can have path-based indexes
(`CREATE INDEX ON vehicles ((data->>'vin'))`). These require the descriptor to carry index
metadata beyond the column name.

**Current spec state:** P7V2 mentions "Phase 6 `IndexSpec::extension_dependency`" and "JSONB
index metadata" in the in-scope list but does not show the descriptor shape for JSONB indexes.

**Action required:** Specify the `IndexSpec` extension for JSONB path expressions, or document
that JSONB path indexes are handled via the raw escape hatch and are not auto-generated.

**Sources:** T11 §JSONB index handling, T12 §Djogi-specific types.

---

### G-16 · Missing-migration-file policy [PRIORITY-2]

**Research finding (T01):** When the ledger records a migration as applied but the migration file
is missing from disk (e.g., it was accidentally deleted or the submodule is stale), Djogi needs
a defined behavior:

- Flyway: records as `MISSING_SUCCESS` state; blocks further migration; requires repair
- Refinery: aborts with an error (`abort_missing`)
- SQLx migrate: no explicit handling; behavior undefined

T01 recommends the Flyway approach: a distinct `MISSING` state in the ledger that blocks forward
migration and surfaces clearly in `plan show`.

**Current spec state:** Not addressed in any spec document.

**Action required:** Specify the missing-migration-file detection and blocking behavior.

**Sources:** T01 §Missing file handling.

---

### G-17 · build.rs file-write vs. warning-only distinction [PRIORITY-2]

**Research finding (T12):** Two distinct behaviors are possible from `build.rs` on drift
detection:

1. **Warning-only:** Emit the compiler diagnostic; do not write files. Developer must run
   `cargo djogi makemigrations` to generate files.
2. **File-write:** Write the migration SQL files to `migrations/` directly; emit the diagnostic
   pointing to the generated files.

T12 identifies the file-write approach as causing IDE churn (editors re-read generated files on
every build, triggering unnecessary file watches).

**Current spec state:** SPEC-M §10.2 says "Generates a migration pair ... into `migrations/`"
— implying file-write from `build.rs`. SPEC-D says "Migration generation — Automatic via
`build.rs`". P7D §Core Model says "`build.rs` may read the snapshot. It must never mutate it."
(referring to the snapshot; it does not say build.rs cannot write migration files.)

**Why it matters:** The chosen behavior affects developer workflow and IDE integration. These two
statements need reconciliation.

**Action required:** Clarify whether `build.rs` writes migration files or only emits diagnostics.
If file-write, specify file-write atomicity. If warning-only, update SPEC-M to match.

**Sources:** T12 §build.rs IDE-churn, SPEC-M §10.2, P7D §Core Model.

---

### G-18 · Snapshot merge conflict resolution [PRIORITY-3]

**Research finding (T11):** When two branches both generate migrations (e.g., two developers
each add a model field), the `schema_snapshot.json` will have a merge conflict on `git merge`.

T11 notes that Django's migration dependency graph (`dependencies = [...]`) makes merge
conflicts user-visible and resolvable. Djogi's JSON snapshot does not have a built-in merge
strategy.

T11 recommends:
- The snapshot format should be versioned from day one (like Liquibase's `V:hex`)
- `cargo djogi migrate` should validate snapshot coherence before planning
- A `cargo djogi reconcile` or snapshot-repair workflow should handle post-merge conflicts

**Current spec state:** SPEC-M shows the snapshot format without a format version field.
`schema_snapshot.json` example has `"version": "0005"` (migration version, not format version).

**Action required:** Add a `format_version` field to `schema_snapshot.json`. Specify the merge
conflict resolution workflow (user must run `makemigrations` after merge to produce a merged
migration; the snapshot is rebuilt from the merged migration set).

**Sources:** T11 §Snapshot merge conflicts.

---

### G-19 · `djogi verify` command not in CLI spec [PRIORITY-3]

**Research finding (T11):** T11 recommends a `djogi verify` command that:

1. Reads the current snapshot
2. Connects to the live database
3. Compares snapshot-declared schema against live catalog
4. Reports discrepancies without modifying anything

This bridges the gap left by rejecting the shadow database approach (Prisma's `prisma migrate
diff`). It is especially useful after manual DDL changes or after baseline adoption.

**Current spec state:** P7V2 §CLI Surface does not include `djogi verify`. The closest command is
`cargo djogi plan` (which checks for pending migrations, not live catalog drift).

**Action required:** Add `cargo djogi verify` to the CLI spec. Define its output format and what
constitutes a verification failure.

**Sources:** T11 §djogi verify, P7D §Non-Goals (inspectdb is deferred; verify is different).

---

### G-20 · Prisma's `HistoryDiagnostic` taxonomy not adopted in Djogi [PRIORITY-3]

**Research finding (T01):** T01 recommends adopting Prisma's three-state diagnostic taxonomy for
plan output:

- `DatabaseIsBehind` — pending unapplied migrations
- `MigrationsDirectoryHasUnexpectedHistory` — files present not in ledger, or ledger entries
  with no matching file
- `HistoryDiverged` — ledger and files conflict (checksum mismatch, out-of-order records)

**Current spec state:** P7D §Operator Model describes a workflow but does not define a structured
diagnostic taxonomy. P7V2 §`cargo djogi plan` describes plan output informally.

**Action required:** Adopt a structured diagnostic taxonomy (Prisma-style or adapted) for `plan`
and `show` output. Specify which diagnostics are errors (block migration) vs. warnings (proceed
with confirmation).

**Sources:** T01 §HistoryDiagnostic taxonomy.

---

### G-21 · `NULLS NOT DISTINCT` support not addressed [PRIORITY-3]

**Research finding (T08):** Postgres 15+ supports `CREATE UNIQUE INDEX ... NULLS NOT DISTINCT`,
which treats multiple NULL values as equal for uniqueness purposes. No surveyed Rust system
supports this. It is directly relevant to partial unique constraint patterns.

Since Djogi targets Postgres 18 (well above Postgres 15), this feature is always available.

**Current spec state:** Not mentioned in any spec.

**Action required:** Decide whether `NULLS NOT DISTINCT` is Phase 7 scope or post-0.1.0.
If Phase 7, add it to composite index attribute syntax (G-09). If deferred, document explicitly.

**Sources:** T08 §NULLS NOT DISTINCT.

---

## Part IV: Contradictions

Contradictions are places where two spec documents make mutually exclusive statements. The Phase 7
design document (P7D) is the more recent and more deliberate document; wherever it conflicts with
older spec documents, P7D's position should be treated as the design target unless the conflict is
resolved by an explicit decision in `docs/spec/decisions.md`.

### C-01 · sqlx runner vs. Djogi-owned runner [CRITICAL]

**Older spec statement (SPEC-M §10.1):**
> "Execution is sqlx's built-in runner — checksummed, tracked in `_sqlx_migrations`"

**Phase 7 design statement (P7D §Runner Ownership):**
> "No compatibility dependency on `sqlx::migrate` should survive into the real Phase 7 system.
> Phase 5-Zero already moved Djogi to `tokio-postgres` / `deadpool-postgres`, so the migration
> system should not reintroduce `sqlx` at the most operationally critical layer."

**Phase 7 v2 plan statement (P7V2 §Critical Design Decision 1):**
> "No `sqlx::migrate` compatibility layer survives into the real migration system. Phase 7 ships
> a Djogi-owned planner, SQL emitter, ledger, and runner."

**Resolution:** P7D and P7V2 are authoritative. SPEC-M §10.1 is superseded. The table name
changes from `_sqlx_migrations` (sqlx's convention) to `djogi_schema_migrations` (Djogi-owned).

**Action required:** Update SPEC-M §10.1 to remove the sqlx runner reference and replace it with
the Djogi-owned runner statement. Add `djogi_schema_migrations` ledger table name as a locked
decision in SPEC-D.

**Sources:** SPEC-M §10.1, P7D §Runner Ownership, P7V2 §Critical Design Decision 1.

---

### C-02 · `_sqlx_migrations` table name vs. `djogi_schema_migrations`

**Older spec statement (SPEC-M §10.1):** Implies `_sqlx_migrations` as the ledger table (by
referencing sqlx's built-in runner which exclusively uses that name).

**Phase 7 v2 plan statement (P7V2 §Ledger shape):**

```sql
CREATE TABLE IF NOT EXISTS djogi_schema_migrations ( ... );
```

**Resolution:** `djogi_schema_migrations` is correct. The `_sqlx_migrations` table name should
never appear in Djogi migration documentation.

This is a consequence of C-01 rather than an independent contradiction, but it is worth flagging
separately because it directly affects any existing test or documentation that references
`_sqlx_migrations`.

**Action required:** Search all Djogi documentation for `_sqlx_migrations` and replace with
`djogi_schema_migrations`. Audit any existing code or test fixtures that reference the old name.

**Sources:** SPEC-M §10.1, P7V2 §Ledger shape DDL.

---

### C-03 · SchemaDelta completeness: enum, extension, rename not in SPEC-M

**Older spec statement (SPEC-M §10.6):**

```rust
enum SchemaDelta {
    CreateTable { table: TableDef },
    DropTable { name: String },
    AddColumn { table: String, column: ColumnDef },
    DropColumn { table: String, name: String },
    AlterColumn { table: String, name: String, change: ColumnChange },
    AddIndex { table: String, index: IndexDef },
    DropIndex { name: String },
    AddForeignKey { table: String, fk: ForeignKeyDef },
    DropForeignKey { table: String, name: String },
}
```

**Phase 7 v2 plan statement (P7V2 §In Scope for Phase 7):** The differ must handle:
- enum types and variants
- extensions
- partition metadata
- explicit rename operations (`RenameColumn`, `RenameTable`)
- composite unique constraints (as distinct from indexes)
- JSONB index metadata

**Resolution:** The `SchemaDelta` enum in SPEC-M is an early draft that is now materially
incomplete. The Phase 7 implementation's differ will need a substantially expanded variant list.
The SPEC-M enum is a contradiction only in the sense that it implies completeness it does not
have — implementations reading SPEC-M would build an under-featured differ.

**Action required:** Update SPEC-M §10.6 to add the missing variants, or mark it explicitly as
an early sketch superseded by P7V2. At minimum add: `RenameColumn`, `RenameTable`,
`CreateEnum`, `AlterEnum`, `DropEnum`, `CreateExtension`, `DropExtension`,
`AddUniqueConstraint`, `DropUniqueConstraint`.

**Sources:** SPEC-M §10.6, P7V2 §Canonical Scope.

---

## Part V: Priority Matrix

| # | Item | Type | Priority | Phase | Effort |
|---|---|---|---|---|---|
| G-01 | Advisory lock key derivation | Gap | P1 | Pre-coding | Low |
| G-02 | Checksum algorithm + normalization + version prefix | Gap | P1 | Pre-coding | Low |
| G-03 | Ledger DDL — status, run_id, applied_by, surrogate PK | Gap | P1 | Pre-coding | Low |
| C-01 | sqlx runner vs. Djogi-owned runner (SPEC-M §10.1) | Contradiction | P1 | Immediate | Low |
| C-02 | `_sqlx_migrations` vs. `djogi_schema_migrations` | Contradiction | P1 | Immediate | Low |
| C-03 | SchemaDelta enum incomplete vs. P7V2 scope | Contradiction | P1 | Pre-T1 | Low |
| G-09 | Composite constraint attribute syntax | Gap | P2 | Pre-T1 | Medium |
| G-10 | Composite constraint naming + truncation algorithm | Gap | P2 | Pre-T1 | Medium |
| G-05 | `-- djogi:no-transaction` directive specification | Gap | P2 | Pre-T3 | Low |
| G-06 | Snapshot invariant under partial failure | Gap | P2 | Pre-T4 | Medium |
| G-07 | Rollback ordering by installed_rank | Gap | P2 | Pre-T4 | Low |
| G-11 | Destructive classifier operation-to-bucket mapping | Gap | P2 | Pre-T3 | Low |
| G-13 | `#[model(renamed_from)]` not in base spec | Gap | P2 | Pre-T1 | Low |
| G-14 | Partial/functional index descriptor representation | Gap | P2 | Pre-T2 | Medium |
| G-15 | JSONB subfield index descriptor representation | Gap | P2 | Pre-T2 | Medium |
| G-16 | Missing-migration-file policy | Gap | P2 | Pre-T4 | Low |
| G-17 | build.rs file-write vs. warning-only distinction | Gap | P2 | Pre-coding | Low |
| G-04 | deadpool advisory lock release behavior | Gap | P2 | Pre-T5 | Low |
| G-08 | Lifecycle of `#[field(renamed_from)]` after migration | Gap | P3 | Pre-T7 | Low |
| G-12 | Warnings embedded in generated UP SQL as comments | Gap | P3 | Pre-T3 | Low |
| G-18 | Snapshot merge conflict resolution strategy | Gap | P3 | Pre-0.1.0 | Medium |
| G-19 | `djogi verify` command not in CLI spec | Gap | P3 | Pre-0.1.0 | Medium |
| G-20 | Prisma HistoryDiagnostic taxonomy not adopted | Gap | P3 | Pre-T6 | Low |
| G-21 | `NULLS NOT DISTINCT` support not addressed | Gap | P3 | Post-0.1.0 | Low |

**Phase column legend:**
- `Pre-coding` — must be decided before any Phase 7 code is written
- `Pre-T1` through `Pre-T8` — must be decided before the corresponding P7V2 task
- `Pre-0.1.0` — must be decided before the 0.1.0 release
- `Post-0.1.0` — can be addressed after 0.1.0

**Effort column legend:**
- `Low` — one spec decision, a few sentences, no implementation cost
- `Medium` — requires a new attribute syntax, format extension, or non-trivial spec section

---

## Part VI: Actions for `14-locked-recommendations.md`

`14-locked-recommendations.md` should produce one explicit recommendation per action item below.
Each recommendation should be a locked decision (adopt/reject/defer + rationale + exact spec
text to add), not an open question.

### Batch A: Must Lock Before Phase 7 Implementation Begins

**A-1 (from G-01):** Choose and lock the Djogi advisory lock key value. Recommended: derive a
stable `BIGINT` constant from the ASCII bytes of `DJOGMIGR` (or similar Djogi-specific string)
that is verifiably different from Prisma's `72707369` and outside Flyway's range. Document the
chosen value and the collision analysis in the spec.

**A-2 (from G-02):** Lock the checksum specification: SHA-256 over UTF-8 SQL bytes with BOM
stripped and line endings normalized to `\n`. Storage format: `V1:<64-hex-chars>`. Document the
normalization rules exactly. Prohibit hashing filename or version string into the content hash
(anti-pattern established by refinery).

**A-3 (from G-03):** Finalize ledger DDL by adding `status`, `run_id`, `applied_by`, and surrogate
BIGINT primary key. Lock the `status` enum values: `('pending', 'applied', 'failed',
'rolled_back')`. Document the Prisma pre-write row pattern (INSERT with `status = 'pending'`
before migration runs; UPDATE to `status = 'applied'` or `'failed'` after). Remove any reference
to `source_checksum` as an anti-recommendation.

**A-4 (from C-01, C-02):** Update SPEC-M §10.1 to explicitly state: "Execution is Djogi's owned
migration runner over `tokio-postgres`; the ledger table is `djogi_schema_migrations`." Remove
"sqlx's built-in runner" and "`_sqlx_migrations`" from all spec documents.

**A-5 (from C-03):** Update SPEC-M §10.6 `SchemaDelta` enum to include: `RenameColumn`,
`RenameTable`, `CreateEnum`, `AlterEnum`, `DropEnum`, `CreateExtension`, `DropExtension`,
`AddUniqueConstraint`, `DropUniqueConstraint`. Mark the enum as the complete Phase 7 surface.

**A-6 (from G-17):** Decide and lock whether `build.rs` writes migration files to disk or only
emits diagnostics. If file-write: specify write atomicity (write to `.tmp`, rename atomically).
If warning-only: update SPEC-M §10.2 accordingly.

### Batch B: Must Lock Before Phase 7 Implementation Reaches T1/T2

**B-1 (from G-09):** Lock composite constraint/index attribute syntax in SPEC-MO. Recommended
form: model-level `#[model(indexes(...))]` attribute that accepts `unique(col_a, col_b)` and
`index(col_c)` entries. Document that field-level `#[field(unique)]` and `#[field(index)]` remain
for single-column cases.

**B-2 (from G-10):** Lock naming convention for composite constraints and indexes. Recommended:
`<table>_<col1>_<col2>_key` for unique constraints, `<table>_<col1>_<col2>_idx` for indexes.
On name exceeding 63 bytes: SHA-256 of the full name, take first 8 hex characters as suffix,
truncate to 63 bytes total.

**B-3 (from G-13):** Add `#[model(renamed_from = "old_table")]` to SPEC-MO and SPEC-D as a
locked annotation mirroring the field-level `#[field(renamed_from)]` decision.

**B-4 (from G-14):** Decide partial/functional index scope. Recommended: Phase 7 supports the
predicate field (`where_clause: Option<String>`) and expression field (`expression: Option<String>`)
in `IndexSpec`, with SQL emission for the common cases. Document that complex expression indexes
(referencing multiple columns with custom functions) require the raw escape hatch for this release.

**B-5 (from G-15):** Specify the `IndexSpec` extension for JSONB path indexes. Recommended:
`json_path: Option<String>` on `IndexSpec` that, when present, generates
`CREATE INDEX ... ON t ((col->>'path'))` rather than a plain column index.

### Batch C: Must Lock Before Phase 7 T3/T4 Implementation

**C-1 (from G-05):** Specify the `-- djogi:no-transaction` directive: must appear on the first
non-blank, non-comment line of the file; causes the runner to treat the entire file as a single
non-transactional segment; can appear independently in up and down files; does not suppress
structured segment detection for auto-split migrations.

**C-2 (from G-06):** Specify the snapshot partial-failure invariant: on failure of a
non-transactional segment, write a `migrations/.migration_failure.json` marker with the
failed version, failed segment index, and expected next snapshot version. The runner refuses to
plan or apply until the marker is cleared by `cargo djogi migrate repair`.

**C-3 (from G-07):** Lock rollback ordering as `installed_rank` order (reverse-temporal). The
ledger `installed_rank` is a monotonically increasing counter set at INSERT time. Rollback walks
from highest `installed_rank` to lowest, setting `status = 'rolled_back'` in order.

**C-4 (from G-11):** Publish the operation-to-bucket table in SPEC-M. Key decisions: DROP TABLE
and DROP COLUMN go to `unexecutableSteps` (require `--allow-destructive`). Type narrowing and
index drops go to `warnings` (proceed with explicit warning comment in SQL). Enum value deletion
and nullability tightening without prior backfill go to `unexecutableSteps`.

**C-5 (from G-16):** Specify missing-migration-file behavior: on ledger entry present but file
absent, mark as `MISSING` state in diagnostics; block further migration apply; require
`cargo djogi migrate repair` with operator confirmation to proceed.

**C-6 (from G-04):** Specify runner connection lifecycle: the migration runner acquires a
dedicated single connection (not from the pool) for the duration of migration apply. The
connection is released on success, failure, or process signal. Use `pg_try_advisory_lock` with a
configurable wait timeout (default: 30 seconds, configurable in `Djogi.toml`).

### Batch D: Must Lock Before Phase 7 T6/T7 (Repair and CLI)

**D-1 (from G-08):** Lock lifecycle of `#[field(renamed_from)]`: recommended decision is
migration-window-only marker. The annotation must be removed after the rename migration is
generated and applied. The differ must detect the stale annotation (present in code, already
reflected in snapshot) and emit an error instructing the developer to remove it. This prevents
silent double-rename migrations.

**D-2 (from G-12):** Add destructive/lossy warning comments to generated UP files (not just DOWN
files). Format: `-- DJOGI WARNING: <classification> — <plain-English description>` on the line
immediately before the statement.

**D-3 (from G-20):** Adopt Prisma-style `HistoryDiagnostic` taxonomy for `plan` and `show`
output. Specify three top-level diagnostic states: `DatabaseIsBehind`, `UnexpectedHistory`,
`HistoryDiverged`. Each maps to a specific set of conditions, a default action (block or warn),
and an override flag.

### Batch E: Must Lock Before 0.1.0 Release

**E-1 (from G-18):** Specify snapshot merge conflict resolution: add `format_version` field to
`schema_snapshot.json` format. After a branch merge that causes a snapshot conflict, instruct
the developer to run `cargo djogi makemigrations` which re-derives the merged snapshot from the
ordered applied migrations plus the new desired state. Document this in developer docs.

**E-2 (from G-19):** Add `cargo djogi verify` to CLI spec. Behavior: connect to the target
database, compare live catalog to `schema_snapshot.json`, report columns/indexes/constraints
present in one but not the other. Exit code 0 if identical, non-zero if any discrepancy. Does
not modify any state.

### Batch F: Deferred to Post-0.1.0

**F-1 (from G-21):** `NULLS NOT DISTINCT` support: defer to post-0.1.0. Document as a known
gap with a reference to the Postgres 15+ feature. The composite index attribute syntax (B-1)
should reserve space for a future `nulls_not_distinct: bool` option.

---

*End of gap analysis.*
