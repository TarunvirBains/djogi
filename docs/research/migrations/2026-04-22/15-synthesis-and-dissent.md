# Synthesis and Dissent: Closing Reflections

**Status:** closing document for the 2026-04-22 migration research
**Context:** 11 systems × ~400-800 lines of project notes = 5,496 lines; 12 topic syntheses =
6,805 lines; gap analysis = 1,490 lines; recommendations = 1,736 lines. Total: ~15,500 lines.

---

## Executive Summary

This document closes the 2026-04-22 migration research series. The eleven systems surveyed were
Flyway, Liquibase, Prisma, Alembic, Django, SQLAlchemy, Diesel, SeaORM, sea-query, refinery, and
cot. Across twelve topic syntheses, one gap analysis, and one locked-recommendations document,
the research answered a specific question: what can production migration systems teach Djogi before
it builds its own?

The short answer is more useful than it first appears. On the fundamentals — ledgers, checksums,
locking, transaction semantics, destructive classification — every system that has been run at
scale converged on the same answers. Djogi can read those convergences as load-bearing evidence:
the choices are not arbitrary; they are what survived contact with production. On a smaller set
of questions — source of truth, diff algorithm, rename handling, repair tooling — the systems
split along irreducible lines that reflect genuine differences in philosophy, era, and target
audience. Djogi chose a position on each split, and in most cases that position is grounded by at
least one production system running the same bet for years.

If a reviewer opens only this document, the essential takeaways are: (1) Djogi's descriptor-canonical
architecture, SHA-256 checksums with format versioning, session-scoped advisory locking, and
per-migration transaction model are all individually validated by prior art; (2) the Rust
migration ecosystem is materially behind Python on autogeneration quality, and nobody in any
ecosystem has fully automated online-safe migrations — Djogi is not behind, the field simply has
not solved these yet; (3) five questions remain genuinely open, not because the research was
incomplete, but because no system has good prior art for the specific Djogi constraint, and honest
acknowledgement of that is more useful than a fabricated answer.

---

## Part I: Universal Convergence

These are patterns every surveyed system agreed on with little or no variance. The convergence
is strong enough to treat each as a near-axiom — not a design choice Djogi is making, but a
constraint the migration-systems problem space imposes.

### C-01: Every system tracks a ledger in the database

Without exception. Alembic's `alembic_version` is a single-column current-state pointer, not a
log — but it still exists in the database. Refinery's `refinery_schema_history` contains a
checksum column no other Rust system bothers with. SeaORM's `seaql_migrations` stores only
`version` and `applied_at` as a BIGINT unix timestamp (a format choice every other system
implicitly judges as a mistake). But all eleven maintain *something* in the database to answer
"which migrations ran here." (T02)

The minimum viable ledger — proven by SeaORM and cot — is two columns: a migration identity
string and a timestamp. Everything beyond that is operational quality. Djogi's planned twelve-column
ledger is not overengineering; it is the accumulation of every painful production gap the minimalist
systems left open: no checksum → no drift detection; no status column → no crash-window recovery;
no `run_id` → no deployment-level audit; no `applied_by` → no post-incident accountability.

### C-02: Every system requires explicit migration ordering

No system uses content-based or dependency-inferred ordering. All eleven systems require the user
to declare migration sequence explicitly — through sequential file prefixes (Flyway's `V1__`, `V2__`),
integer versions (refinery's `V1_name.sql`), a DAG of `down_revision` pointers (Alembic), or the
order of items in a `Vec<Box<dyn MigrationTrait>>` (SeaORM). (T01, T06, T11)

The reason is not conservatism — it is that partial-ordering of schema operations is genuinely
ambiguous. Adding column A before column B or B before A produces different valid schemas if both
are `NOT NULL DEFAULT`. A topological sort on SQL dependencies would require a full SQL parser to
enumerate dependencies, and even then the ordering of independent operations is arbitrary.
Explicit sequencing is the simplest contract that works.

### C-03: Every system defaults to per-migration transactions on Postgres

Eight of nine Postgres-applicable systems run each migration inside its own `BEGIN`/`COMMIT`.
The one exception is refinery, which runs migration SQL and the ledger INSERT as two separate
transactions by default — a design the research identified as the single largest operational gap
in the Rust ecosystem (T05). Alembic's default wraps *all* migrations in one transaction when
`transaction_per_migration=False` (which is the default), but this collapses to the same pattern
when using `transaction_per_migration=True`.

The convergence is forced by Postgres's DDL-in-transaction support. Unlike MySQL, Postgres can
roll back `CREATE TABLE` and `ALTER TABLE`. Any system that does not use per-migration transactions
on Postgres is leaving this rollback guarantee on the floor. (T05)

### C-04: Every autogenerate system matches columns by name, not position

No surveyed system matches a column in the model against a column in the database by ordinal
position. All eleven match by name. The implication is that reordering fields in a Rust struct or
Python model produces no migration — which is correct behavior. (T11)

The alternative — positional matching — would make struct field order semantically load-bearing
and break every existing migration corpus whenever a developer added a field in the middle of
a struct. No system has ever shipped positional matching.

### C-05: No system automates online-safe migrations by default

The ecosystem has unanimously decided that zero-downtime DDL (`CREATE INDEX CONCURRENTLY`,
`NOT VALID` constraint addition, expand-contract column sequences) is the operator's
responsibility. Django's `AddIndexConcurrently` is opt-in. Flyway auto-detects that a script
contains `CONCURRENTLY` and strips the transaction wrapper — but the user wrote `CONCURRENTLY`
themselves. Prisma's Rust engine splits per-statement to allow concurrent index creation — but
the user wrote the statement. Nobody generates the two-phase expand-contract pattern
automatically. (T10)

This is not a gap anyone is embarrassed about. Automating online-safe migrations requires
per-operation lock-analysis across the full migration plan, knowledge of table sizes, and the
ability to generate multi-step operations from a single model change. Phase 7.5 of Djogi is
planned as the attempt; the research found that even Prisma, the most technically ambitious
system in the survey, stops well short of full automation here.

### C-06: No system is data-aware during migration generation

All destructive-operation classification is syntactic: `DROP COLUMN` is lossy because the
syntax says so, not because the system ran a `COUNT(*)` query to determine whether the column
has any values. Prisma is the one exception at the *evaluation* stage — `evaluateDataLoss` runs
live `COUNT(*)` probes against the production database — but this is not run during autogeneration
of the migration file; it is run as a separate command before apply. The generation step is always
syntactic-only. (T09)

The practical consequence is that an autogenerated `DROP COLUMN` on an empty table looks
identical to one on a table with a million rows — same SQL, same warning level. No system says
"this drop is safe because the column has zero non-null values." This is a known limitation
accepted by every system in the survey.

### C-07: Every system that checksums uses hex strings in TEXT-compatible columns

Where checksums exist (Flyway, Liquibase, Prisma, refinery), the stored format is always a
hex string in a VARCHAR or TEXT column. No system uses a binary column for checksum storage.
Flyway's CRC-32 is stored as a signed `INTEGER` (the worst format in the survey, because Java's
cast of the unsigned CRC value to `int` produces negative numbers), but even Flyway's engineers
knew this was a mistake — the documentation warns about it. Every more recent system chose a
hex string. (T02, T03)

### C-08: Every system with a repair or stamp command requires explicit operator action

No system silently auto-repairs a checksum drift, a missing migration file, or a partial-apply
state. Flyway's `repair` must be explicitly invoked. Prisma's `migrate resolve` requires `--applied`
or `--rolled-back`. Django's `--fake` is a deliberate choice by the operator. The ecosystem has
implicitly decided that the repair surface is too dangerous to automate without a human in the loop.
(T03, T06)

### C-09: Every system that has concurrent-safe behavior uses a single serializing primitive

Flyway acquires `pg_try_advisory_lock(key)` before reading the pending set. Liquibase updates
`LOCKED=true WHERE LOCKED=false` before reading the changelog. The pattern is universal: the lock
must be acquired *before* reading the pending set, not between the read and the write. Systems
that attempt to rely on the ledger's primary-key uniqueness constraint as the concurrency
mechanism (the six systems with no explicit lock) rely on the database to detect the collision
after both processes have already executed duplicated DDL. The duplicate-key error on the ledger
INSERT is a noisy symptom, not a correctness guarantee. (T04)

### C-10: Every migration file format is versionable independently of the application

Every system — even cot's Rust-code migrations — maintains migration history as a set of discrete,
independently applicable artifacts. No system conflates the migration history with the application
code to the point where a migration cannot be identified, replayed, or audited without the application
binary. This is why SQL migration files are the right primary artifact for Djogi: they are
inspectable without any Djogi tooling. (T01, T12)

---

## Part II: Irreducible Dissent

These are points where the eleven systems split along lines that reflect genuine philosophical
disagreement, different eras of design, or different target audiences. Djogi chose a position on
each. The choice is defensible, but so is the alternative.

### D-01: Where does the canonical desired schema live?

- **Cluster A — ORM model code:** Django (`models.py`), cot (`#[model]` structs), SeaORM
  (ordered migration `Vec`, but no autogen). The schema is what the live application code says.
  Advantage: no separate descriptor file to keep in sync. Risk: if the developer changes a model
  without running `makemigrations`, the divergence is invisible until CI catches it.
- **Cluster B — Migration files:** Flyway, Liquibase, refinery, Diesel. The schema is the
  cumulative result of applying all migration files. Advantage: extremely simple — there is no
  separate model layer to maintain. Risk: the live database can drift from what the migration
  history implies with no detection mechanism.
- **Cluster C — Declarative descriptor file:** Prisma (`schema.prisma`), Djogi
  (`target/djogi_models.json` built from `#[djogi::model]` structs). The descriptor is the single
  source of truth; migration files are derived, immutable history. Advantage: the descriptor is
  independently reviewable in a PR. Risk: the descriptor and the migration history can silently
  diverge if a migration is manually edited after generation.
- **Cluster D — Hybrid:** Alembic (SQLAlchemy `MetaData` + migration files maintained in parallel),
  Diesel (`schema.rs` reflection + hand-written SQL). Advantage: flexible. Risk: the two halves
  can drift independently with no enforcement.

Why irreducible: this is a philosophy call about who the primary user is. ORM-canonical optimizes
for developers who think in model terms. Migration-file-canonical optimizes for DBAs who think in
SQL terms. Descriptor-canonical optimizes for teams that want explicit version-control review of
schema intent. Hybrid is a pragmatic compromise that accepts some inconsistency in exchange for
flexibility. No single answer is universally correct.

Djogi chose Cluster C. Prisma's five-year production track record at scale validates the bet.
(T01, T11)

### D-02: Checksum strength

- **SHA-256 (cryptographic):** Prisma only. 256-bit collision space; effectively impossible
  to forge accidentally or deliberately.
- **MD5 with format versioning:** Liquibase. Cryptographically broken but practically adequate;
  the format-versioned `V:hex` string is the most important design contribution of the four.
- **CRC-32:** Flyway. 32-bit collision space; stored as a signed Java `int` (can be negative);
  not cryptographic.
- **SipHash-1-3:** refinery. Non-cryptographic keyed hash; more collision-resistant than CRC-32
  in practice; the anti-pattern of hashing `name + version + sql` means a file rename changes the
  checksum even when SQL is unchanged.
- **None:** Django, Alembic, Diesel, SeaORM, cot. Post-apply mutation of migration files is
  undetectable.

Why irreducible: different threat models. "None" systems have decided that migration file integrity
is a process problem (code review, git history). CRC-32/SipHash systems have decided that
accidental drift is the primary risk (not adversarial tampering). SHA-256 systems have decided
that even accidental drift should be detectable by a strong hash.

Djogi chose SHA-256 with Liquibase's format-versioned `V1:hex` prefix pattern — the strongest
algorithm with the best forward-compatibility design. (T03, R-05)

### D-03: Advisory lock vs. lock table vs. no lock

- **Postgres advisory lock:** Flyway (modern default), Prisma. Auto-released on backend
  termination; Postgres-native; no extra table required.
- **Dedicated lock table:** Liquibase. Survives across database connections but does NOT
  auto-release on crash; requires manual `releaseLocks` to recover from a stuck lock.
- **No lock:** Django, Alembic, refinery, Diesel, SeaORM, cot. Concurrency safety depends
  on the primary-key uniqueness constraint catching a duplicate ledger INSERT after both
  processes have already executed duplicated DDL.

Why irreducible: Liquibase's lock table exists because Liquibase must support non-Postgres
databases that lack advisory locks. For a Postgres-only system like Djogi, the advisory lock
is strictly better on every dimension. The "no lock" camp is not making an architectural choice —
it is omitting a safety mechanism that the project notes confirm was never discussed in the
source code. (T04)

Djogi chose Postgres advisory locks with the key `0x444A4F474D494752` (ASCII `DJOGMIGR`),
distinct from Prisma's hardcoded `72707369` and outside Flyway's range. (R-03)

### D-04: Diff source — how is "applied state" derived?

- **In-memory replay (Django, cot):** Walk all migration files and replay operations against an
  in-memory schema representation. O(n) cost in migration count. Deterministic but can diverge
  from the actual live database if migrations were applied out of order or if manual DDL ran.
- **Live DB introspection (Alembic, Diesel `--diff-schema`, Liquibase):** Connect to the live
  database and introspect `pg_catalog`. Accurate by definition for what the DB has, but requires
  a live connection at diff time and is slow on large schemas.
- **Shadow DB (Prisma):** Apply all migrations to a disposable temporary database, then introspect
  the result. The most accurate approach — detects drift between the migration history and the
  live database — but requires `CREATE DATABASE` permission and a full schema replay on every diff.
- **Stored snapshot (Djogi, cot for the "from" side):** A side-car file (`schema_snapshot.json`)
  records the schema state after the last successful migration. Diff against the snapshot, not
  against the live database. O(1) cost; deterministic; no live connection required at diff time.

Why irreducible: each approach trades off accuracy, cost, and operational complexity differently.
Shadow DB is most accurate but operationally expensive. Live introspection is accurate but slow
and requires a connection. In-memory replay is cheap but can diverge. Stored snapshot is the
cheapest approach but cannot detect out-of-band DDL changes to the live database.

Djogi chose stored snapshot — closest to cot's approach but with the snapshot as a separate
side-car file rather than embedded in migration files. The tradeoff (cannot detect live DB drift
without a `cargo djogi verify` command) is explicit and documented. (T11, G-19)

### D-05: Rename detection — heuristic vs. interactive vs. explicit vs. none

- **Interactive (Django):** `makemigrations` prompts the user at generation time. Correct in
  development; silently destructive when `--no-input` defaults the prompt to "no rename."
- **Heuristic (nothing in the survey — the common assumption that Alembic has heuristic rename
  detection is false):** T07 confirmed that no surveyed system applies a similarity heuristic
  to rename detection in its autogenerate path. Not one.
- **Explicit annotation (Djogi, Django for the final form):** The developer annotates the
  renamed field with `#[field(renamed_from = "old_name")]`. Zero false positives; requires
  developer awareness.
- **None (Prisma, Alembic, Diesel, cot, refinery, Flyway, Liquibase, SeaORM):** A rename is
  treated as `DROP + CREATE`. Data loss is the default unless the operator hand-edits the
  generated SQL.

Why irreducible: interactive detection does not work in CI. Heuristic detection has a nonzero
false-positive rate and no system has been willing to ship it. Explicit annotation requires
developer discipline. None is safe but puts the burden entirely on the operator.

Djogi chose explicit annotation (`#[field(renamed_from)]`, `#[model(renamed_from)]`), validated
as the universal "safest default" position across the research. (T07, R-16, R-20)

### D-06: Repair tooling completeness

- **Full repair (Flyway):** Three distinct operations — delete failed rows, insert tombstones for
  missing successful migrations, in-place update of checksum drift — wrapped in a single `repair`
  command. The in-place checksum update is the one weakness (it silently rewrites history without
  an audit record).
- **State-machine repair (Prisma):** `migrate resolve --applied` and `--applied --rolled-back`
  with refusal semantics: will not stamp an already-successful migration; will not delete existing
  rows. Every change produces a new row rather than mutating an existing one.
- **Blunt instrument (Liquibase):** `clearChecksums` issues `UPDATE DATABASECHANGELOG SET MD5SUM = NULL`
  with no filtering, no dry-run, and no audit trail.
- **None (Alembic, Django, Diesel, SeaORM, refinery, cot):** Recovery is entirely manual.

Why irreducible: repair tooling is expensive to build and most teams never use it in the expected
case. The systems without repair tooling are not wrong to omit it — they are making a bet that
their users will not need it. The systems that built it (Flyway, Prisma) are making a bet that
production incidents are inevitable and first-class recovery tooling matters.

Djogi's repair design draws from both Flyway (three operations) and Prisma (state-machine
semantics, no deletion of existing rows). The specific anti-pattern to avoid is Flyway's
in-place checksum update, which rewrites history without an audit record. (T03)

---

## Part III: What the Research Could Not Answer

These are questions the research surfaced but could not resolve from prior art — either because
no system has addressed them, or because the Djogi constraint is genuinely unique.

### Q-01: How to detect advisory lock staleness from a dead connection pool?

Postgres automatically releases session-scoped advisory locks when the backend terminates. But
`deadpool-postgres` keeps connections alive across requests. If the Djogi application process
crashes while holding a migration lock, the pool manager may keep the backend alive — keeping the
lock held — until the pool's connection lifetime expires. No surveyed system addresses this
scenario directly. Prisma's 10-second timeout (`ADVISORY_LOCK_TIMEOUT`) is the closest answer,
but it does not address the pool keep-alive case.

The research-recommended mitigation (R-23): the migration runner uses a dedicated single
`tokio-postgres` connection, not a pool connection, for the duration of the migration apply. When
the runner exits abnormally, the OS tears down the TCP connection and Postgres releases the lock.
But this is still imperfect if the pool manager survives.

The operator escape hatch — query `pg_stat_activity` for idle backends holding the Djogi advisory
lock key, then `pg_terminate_backend(pid)` — is documented in T04 but not yet in any Djogi spec
document. It should be in the operator guide before v0.1.0 ships. (T04, G-04)

### Q-02: How should migrations interact with Postgres logical replication?

Logical replication (via `pg_publication` / `pg_subscription`) copies row-level changes from a
primary to a subscriber. Schema changes propagate only within explicit `ALTER PUBLICATION` commands
or when using `wal2json` / `pglogical` with DDL capture. A `CREATE TABLE` or `ALTER TABLE`
run via `cargo djogi migrate` on the primary will not automatically propagate the schema change
to the subscriber.

No system in the survey addresses this at all. It becomes relevant if Djogi supports
publication-based CDC (change-data-capture) — a pattern common in event-driven architectures.
The research cannot answer this question because no prior art exists. It is flagged here as a
known blank spot for future research, not a current blocker. (T01, T10)

### Q-03: Is the `cargo djogi verify` snapshot-vs-live comparison sufficient for v0.1?

T11 identified that Prisma uses shadow-DB replay to detect drift between the migration history
and the live database. Djogi rejects the shadow-DB approach (it requires `CREATE DATABASE`
permission and a full schema replay) in favor of a `cargo djogi verify` command that compares
`schema_snapshot.json` against `pg_catalog` directly. But this comparison can only detect drift
between the snapshot and the live database — it cannot detect drift caused by a manual DBA running
`ALTER TABLE` directly on a production database without Djogi's knowledge.

The research established that the stored-snapshot approach is O(1) and CI-safe but blind to
out-of-band mutations. The `djogi verify` command (R-24) is the v0.1 answer. Whether it is
*sufficient* depends on operational discipline — whether the team commits to running all DDL
through Djogi. No surveyed system has answered this question definitively; they have simply chosen
their tradeoff and lived with it. (T11, G-19)

### Q-04: What is the right cadence for regenerating `schema_snapshot.json` in the submodule?

The `migrations/` directory is a git submodule managed by CI. When two feature branches both
generate migrations, both branches produce changes to `schema_snapshot.json`. When the branches
merge, there will be a merge conflict on the snapshot. T11 recommends running `cargo djogi makemigrations`
after merge to produce a merged migration and a consistent snapshot. But the mechanics of when
the submodule's snapshot is committed, who commits it, and how to handle the case where both
branches modified the snapshot are not fully specified in any Djogi document.

Neither cot (which embeds snapshots in migration files, avoiding the side-car conflict) nor
Prisma (which uses shadow DB rather than a snapshot file) offers a directly applicable answer.
Djogi must define its own policy. The `format_version` field recommended in R-26 is a prerequisite;
the actual merge workflow is still open. (T11, G-18)

### Q-05: How should the `partial_apply_info` column interact with non-transactional segment resumption?

When a non-transactional migration (e.g., `CREATE INDEX CONCURRENTLY`) fails after the first of
three statements has committed, the runner writes `status = 'failed'`, `partial_apply_state = 'segment_1_of_3'`
to the ledger and writes `.migration_failure.json` to disk. The operator then repairs the database
manually (completing or rolling back statement 1) and runs `cargo djogi migrate repair`.

But the repair command must know which SQL statements were in each segment, in order, to reason
about what has been applied and what has not. If the migration file has been edited between the
initial apply and the repair invocation, the segment boundaries may have changed. No system
surveyed has a complete answer for this case. Prisma's `applied_steps_count` is the most
informative approach — it records how many statements succeeded before failure — but it does not
record which statements they were.

The research cannot resolve this without a complete specification of how Djogi segments a
non-transactional migration file and what the repair command knows about historical segmentation.
(T05, T03, R-07)

### Q-06: Should `#[field(renamed_from)]` be a compiler warning or a blocking error when stale?

R-20 recommends a blocking error when the annotation is stale (the rename is already reflected
in the snapshot). But there is a window between "migration generated" and "migration applied"
where the annotation is present in the source but not yet reflected in the snapshot. During this
window, a subsequent `cargo build` would see a non-stale annotation correctly and emit no error.

The exact lifecycle — at what point the differ transitions from "annotation is current" to "annotation
is stale" — depends on the snapshot update timing. Since the snapshot is updated only on successful
`cargo djogi migrate`, a developer who generates a rename migration but does not apply it will
see no error from the stale-annotation check until after apply. This is correct behavior but
the research did not trace the full lifecycle through every possible intermediate state. (T07, R-20)

### Q-07: What does Djogi do when `djogi_schema_migrations` already exists with a different schema?

The ledger table is created with `CREATE TABLE IF NOT EXISTS`. If an older version of Djogi
(or a manual creation) created the table with a different column set, the `IF NOT EXISTS`
succeeds silently and subsequent operations fail when they try to read or write columns that
do not exist.

Django's approach is to run `schema_editor.create_model()` through its own ORM, which can detect
this. Flyway's approach is to introspect the existing table and emit a helpful error. No Rust
system surveyed handles this case well. The research noted it as an open operational question;
none of the 15 recommendations in doc 14 address it. It should be on the Phase 7 T2 implementation
checklist. (T02)

### Q-08: At what Postgres version does `ALTER TYPE ... ADD VALUE` become safe inside a transaction?

T05 notes that adding a value to an enum type before Postgres 12 cannot be seen within the same
transaction. Postgres 12+ lifted the restriction for most cases. Postgres 18 (Djogi's floor)
is well above 12, so Djogi can treat `ALTER TYPE ... ADD VALUE` as transactional.

But Flyway's implementation (`PostgreSQLParser.java:125-134`) dynamically queries the server version
to decide this at runtime, which is the correct production posture. Djogi targets Postgres 18 as
a permanent floor, so the dynamic check is unnecessary — but the research did not fully verify
whether any edge cases remain in Postgres 18's handling of enum value addition in transactions.
This is a low-risk open question but should be verified before the enum differ is implemented.
(T05, T08)

---

## Part IV: Meta-Observations About the Ecosystem

These are reflective points about what the research revealed about the migration-systems space as
a whole, not about any specific Djogi decision.

### M-01: The Rust migration ecosystem is materially behind Python on autogeneration quality

cot's `migration_generator.rs` hits `todo!()` at line 835 when it encounters a field type change.
SeaORM has no autogeneration at all — users write migration code by hand. Diesel's `--diff-schema`
is labeled experimental and does not handle composite foreign keys. refinery is a raw-SQL runner
with no generation layer whatsoever. Only Prisma — whose autogeneration logic is in a compiled
Rust binary backed by years of TypeScript tooling investment — produces autogenerated SQL that is
trustworthy enough for production use.

Contrast with Alembic, which has been autogenerating `ALTER TABLE` statements from `MetaData`
diffs since 2010. Django's `makemigrations` handles composite indexes, enum types, constraint
changes, and even provides interactive rename detection. The Python ecosystem's autogeneration
story is fifteen years older and correspondingly more mature.

This is not a criticism of the Rust ecosystem — it is an observation about its age. Djogi's
bet is that building the autogeneration system correctly from scratch, informed by fifteen years
of Python prior art, is feasible and valuable. The research supports this bet. But Djogi should
expect to be the most mature autogeneration system in the Rust ecosystem for years. (T12)

### M-02: The "checksum as format-versioned string" pattern is dramatically under-adopted

Liquibase is the only system among eleven that embeds a version prefix in its stored checksum
(`V9:2cdf9876e74347162401315d34b83746`). Every other checksumming system stores a raw hex string
or a raw integer. This means that when Flyway, Prisma, or refinery need to change their checksum
algorithm, they have two options: (1) break every existing ledger row, or (2) maintain forever
the exact algorithm they shipped in v1.

Prisma actually encountered this problem: issue #1887 is a backward-compatibility wrinkle around
zero-padding in the hex output format. The fix was a length-detection hack (`checksum.len() != CHECKSUM_STR_LEN`)
rather than a version prefix. Flyway has used CRC-32 since its first release in 2010 and cannot
change algorithms without breaking every stored checksum.

Liquibase's `V:hex` format costs essentially nothing to implement and eliminates this class of
future technical debt entirely. Djogi adopted it in R-05. The broader observation is that this
pattern is cheap, valuable, and almost universally missed. It should be in every system that
stores checksums. (T03)

### M-03: Partial and functional indexes are systematically underserved in the Rust ecosystem

Django supports `Index(condition=Q(...))` for partial indexes and `Index(F('col').lower())` for
functional indexes. SQLAlchemy supports both via `postgresql_where=` and expression arguments.
Sea-query exposes `.and_where(expr)` for partial indexes.

In Rust: Diesel's `schema.rs`/`table!` macro does not model indexes at all. SeaORM has no
first-class partial or functional index support. cot has no `Operation` type for index creation
of any kind. refinery is raw SQL. Only sea-query (a query builder, not an ORM) exposes
partial and functional indexes as first-class builder API.

Djogi's `IndexSpec` design (R-21) — with `where_clause: Option<String>` and `expression: Option<String>`
for raw-SQL partial and functional index predicates — would make Djogi the only Rust ORM/migration
system with first-class descriptor support for both. This is not a large differentiator in
absolute terms, but it is genuine differentiation in a domain where the Rust ecosystem is notably
weak. (T08)

### M-04: Nobody trusts autogenerate in production without a human review step

Django warns you to review generated migrations. Alembic's `compare_type` (which detects column
type changes) is disabled by default with a comment explaining that type comparison is unreliable.
Prisma requires `migrate dev` to be interactive in non-trivial cases and discourages using
`prisma db push` in production. Flyway has no autogenerate at all — it trusts the user to write
correct SQL.

The ecosystem has implicitly converged on a position: autogenerate is a developer-convenience
tool that produces a *starting point*, not a production-trustworthy artifact. The migration file
is the authoritative document; the autogenerated content is a draft. This is why Djogi's
`build.rs`-emits-diagnostic-only design (R-12) is the correct posture — the developer reviews
the generated SQL before running `cargo djogi migrate`. (T09, T12)

### M-05: Repair is an afterthought in most systems

Flyway's `repair` command and Prisma's `migrate resolve` are genuine outliers. Six of the eleven
systems — Alembic, Django, Diesel, SeaORM, refinery, cot — have no repair command. For those
systems, the recovery workflow when a migration partially applies is: connect to the production
database, manually inspect what DDL succeeded, manually fix the schema, manually insert or delete
rows from the ledger, and pray.

This is not because repair is unimportant — every engineering team that has operated a migration
system at scale has a war story about a partial apply that required manual intervention. It is
because repair tooling is expensive to design correctly (you need a consistent state model,
refusal semantics that prevent accidental history rewriting, and dry-run support) and because
most systems are built in environments where "we do not make mistakes in production" is the de
facto policy rather than "mistakes happen and the tooling should help you recover."

Djogi inherits the design intention to ship first-class repair. The research gives it the two
best blueprints: Flyway's three-operation model and Prisma's state-machine refusal semantics.
(T03)

### M-06: The migration ledger as a deployment audit log is an under-explored design space

Liquibase's `DEPLOYMENT_ID` column — which groups all changesets applied in a single `liquibase update`
invocation — is the only attempt in the survey to treat the migration ledger as a deployment
audit log rather than just a schema history. Flyway's `installed_by` and `installed_on` columns
come closest, but they are per-migration, not per-deployment-group.

The question "which migrations applied in last Friday's deployment?" should be answerable by a
single SQL query on the ledger. For ten of the eleven surveyed systems, it is not — you must
cross-reference the migration timestamps with your deployment logs and hope the clocks are
synchronized. Only Liquibase's `DEPLOYMENT_ID` makes this query trivial.

Djogi's `run_id` column (R-04) directly addresses this gap. It is not a novel idea — it is
Liquibase's decade-old `DEPLOYMENT_ID` pattern, which somehow has not been adopted by any other
system in the survey. (T02)

### M-07: The "schema snapshot" approach to diff source is the most under-explored pattern

The four approaches to deriving "applied state" for diff purposes (in-memory replay, live
introspection, shadow DB, stored snapshot) all appear in production systems. But the stored
snapshot — Djogi's chosen approach — is the least represented in mature systems. cot does
something similar by embedding snapshot structs in migration files, but the full side-car
`schema_snapshot.json` approach is novel in the Rust ecosystem.

The stored snapshot is the approach that optimizes for CI-safety and O(1) cost. It has a known
weakness (cannot detect out-of-band live database mutations). But for a system where all DDL
is expected to flow through the migration runner — which is Djogi's design intent — the weakness
is acceptable. The research found no other system that has built this approach to completion and
evaluated it in production. Djogi is running an experiment that is informed by prior art but not
directly validated by it. (T11)

---

## Part V: Recommendations for Future Research

If someone resumes migration research in six to twelve months, the following tools and projects
did not exist or were not mature enough to survey during this research window. Each represents
a specific open gap identified in the current research.

**pgroll** (Xataka / xataio/pgroll on GitHub) — A Go tool that implements the expand-contract
pattern natively at the database level, using Postgres views and triggers to present both old and
new schema simultaneously during migration. Would directly inform Djogi's Phase 7.5 online-safe
mode design. The research identified zero-downtime DDL as the biggest unsolved problem in the
ecosystem; pgroll is the most serious attempt at a production-grade solution.

**Reshape** (fabianlindfors/reshape on GitHub) — A Rust-native tool with a similar expand-contract
philosophy to pgroll. Worth surveying specifically because it shares Djogi's language ecosystem.
Reshape's approach to presenting both old and new schema during the transition window would inform
how Djogi might generate expand-contract migration pairs rather than forcing operators to hand-write
them.

**Atlas** (Ariga / atlasgo.io) — A Go-based declarative schema management tool whose architecture
is closer to Djogi's descriptor-canonical model than any of the eleven surveyed systems. Atlas's
HCL schema language is analogous to Prisma's PSL; its diff engine produces structured operations
rather than raw SQL. Atlas's approach to lint rules (classifying dangerous operations before they
are applied) is directly relevant to Djogi's two-bucket destructive classifier.

**squawk** (sbdchd/squawk on GitHub) — A Postgres DDL linter that understands lock-acquisition
semantics and can flag DDL statements that will acquire `AccessExclusiveLock` on a live table.
squawk would inform Djogi's lock-classifier implementation in Phase 7.5 — the component that
would annotate generated DDL with its expected lock level and estimated duration.

**Temporal / DBOS** — Not migration tools, but their durable-execution approach to database state
management is relevant to Djogi's partial-apply recovery design. The question of how to resume
a non-transactional migration that failed partway through is structurally similar to the question
of how to resume a durable workflow that failed at step N of M. The durable-execution literature
may offer design patterns that migration tooling has not yet borrowed.

---

## Part VI: Where This Research Contradicted Prior Beliefs

The research confirmed most of the Djogi team's assumptions about migration system design. But
three assumptions did not survive contact with the source code.

**Assumption: "sqlx::migrate might be adequate for Phase 7."**

The `docs/spec/migrations.md` §10.1 as it existed before this research stated: "Execution is
sqlx's built-in runner — checksummed, tracked in `_sqlx_migrations`." The research disproved
this on three independent dimensions: `sqlx::migrate` has no advisory lock (concurrent applies
corrupt the ledger silently), no non-transactional segment awareness (`CREATE INDEX CONCURRENTLY`
fails inside a transaction), and a two-column ledger schema that cannot support Djogi's operational
requirements. The contradiction was flagged as C-01 in the gap analysis and resolved as R-01
in the recommendations document: Djogi owns the runner entirely. (T04, T05, T12)

**Assumption: "Advisory lock key derivation is a detail we can decide later."**

The pre-research posture treated the advisory lock key as an implementation detail. The research
revealed that Prisma hardcoded `72707369` as a magic constant with no derivation rationale and
no documentation that it was chosen. A Djogi key that collided with Prisma's would allow
`prisma migrate` to deadlock `cargo djogi migrate` on shared infrastructure — not a theoretical
concern on developer machines where both tools might run against the same Postgres server. The
key `0x444A4F474D494752` (ASCII `DJOGMIGR`) was locked in R-03 as a Priority-0 item before
Phase 7 coding begins. (T04, G-01)

**Assumption: "cot is the closest Rust analog and we should mostly follow it."**

cot is the closest analog in architectural philosophy — Rust-native, `#[model]` attribute macros,
build-step-driven migration generation. But the research found three cot safety holes that Djogi
must not inherit:
(1) No advisory lock — concurrent `cot migration apply` runs can corrupt the ledger.
(2) No checksum — post-apply mutation of migration files is silent and undetectable.
(3) The snapshot-struct-embedded-in-migration-file design couples the execution plan to the
    snapshot; hand-editing either without updating the other corrupts future diffs. The failure
    mode at `migration_generator.rs:835` — `todo!()` for field type changes — is a direct
    consequence of the snapshot struct design not being able to represent all `ColumnType` variants.

Djogi's design diverges from cot on all three points. The research partially disproved the cot-as-reference
assumption while validating cot's core philosophy. (T12, X-01)

**Assumption: "Checksum algorithms are interchangeable after the fact."**

The team's implicit assumption was that the checksum algorithm was easy to change — if SHA-256
turned out to be overkill, we could always switch to SipHash later. The research showed that
without Liquibase's format-versioned prefix, algorithm changes require updating every stored
checksum in the ledger — a ledger migration that itself needs a runner to apply. Liquibase has
maintained nine checksum algorithm versions since inception; the version prefix is what made
this seamless. The `V1:hex` prefix in R-05 is not optional; it is what enables *all* future
algorithm decisions to be made without a breaking change. (T03)

---

## Part VII: Closing

The research is complete. Twelve topic syntheses, a gap analysis, and a locked-recommendations
document now constitute the primary research record for Djogi's migration system design. The
recommendations in document 14 are organized for review: fifteen P0 items before Phase 7 coding
begins, eleven P1 items before v0.1.0, five deferred items, and five explicit rejections with
documented rationale.

The path forward is straightforward: the user reviews document 14, marks each recommendation as
AGREED, REJECTED, or MODIFIED, and the canonical spec documents (`docs/spec/migrations.md`,
`docs/spec/decisions.md`, `docs/spec/models.md`) are updated to reflect the accepted decisions.
Once the P0 items are locked, Phase 7 T1 implementation can begin with a fully specified target.
The gaps in document 13 map directly to the recommendations in document 14; nothing in either
document is speculative — every item is grounded in source-level evidence from at least one of
the eleven surveyed systems.

The synthesis in this document is the meta-layer above all of that: what did the research reveal
about the migration-systems space as a whole? The answer is that the fundamentals are solved and
converged; the hard problems are at the edges (online-safe automation, multi-region coordination,
the repair surface for non-transactional partial failures); and the Rust ecosystem is young enough
that Djogi's implementation, if done well, will be the reference point for the next generation of
Rust migration tooling rather than a follower of it.

---

*Topic citations: T01 through T12 refer to the corresponding files in
`docs/research/migrations/2026-04-22/topics/`. Document citations: T13 = gap analysis;
T14 = locked recommendations. R-XX citations refer to recommendations in document 14.
G-XX citations refer to gaps in document 13. X-XX citations refer to rejections in document 14.*
