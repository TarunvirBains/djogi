# Locked Recommendations: Djogi Migration System

**Status:** Proposal for review
**Date:** 2026-04-22
**Authority:** Supersedes `docs/spec/migrations.md` §10.1 once accepted; clarifies Phase 7 design doc
(the local Phase 7 migration-system design notes).
**Reading order:** Start at Part I (P0 recommendations) for the critical path. Part II (P1)
follows for items that must land in v0.1 but are not blocking Phase 7 coding. Part III (P2)
captures deferred items. Part IV lists explicit rejections. Part V is the spec-update manifest.
Part VI is the open-items list for user decision.

---

## Executive Summary

This document converts the 12 topic syntheses and the gap analysis in
`13-gap-analysis-vs-current-spec.md` into a flat list of one-decision-per-section recommendations.
Every gap and contradiction from document 13 maps to at least one recommendation here.

The research found **3 live contradictions** in existing spec documents, **21 gaps** of varying
priority, and **29 validated decisions** that need no change. This document proposes concrete
resolutions for all contradictions and all gaps, and records explicit rejections for design patterns
the research considered and found unsuitable for Djogi.

**P0 recommendations (must-have before any Phase 7 code is written): 15**
These resolve C-01, C-02, C-03, G-01, G-02, G-03, G-05, G-06, G-07, G-09, G-10, G-11, G-13,
G-16, G-17 — the full set of pre-coding and pre-T1 items from document 13's priority matrix.

**P1 recommendations (should-have for v0.1 but not blocking Phase 7 coding start): 11**
These resolve G-04, G-08, G-12, G-14, G-15, G-18, G-19, G-20, G-21, and two additional
items identified in the spec update manifest.

**P2 recommendations (defer to v0.2+): 5**
These address items the research considered and explicitly decided to defer, not omit.

**Explicit rejections: 5**
Design patterns from other systems that the research evaluated and decided Djogi must not adopt.

The biggest recommendation by impact is **R-04** (complete ledger DDL), because the ledger schema
is the first artifact the runner will build and every other recommendation touches it.

---

## Acceptance Workflow

Read each recommendation from top to bottom. For each:

1. Read the **Recommendation** paragraph — the actionable statement.
2. Read **Rationale** to understand the research backing.
3. Read **Alternatives considered** to understand what was rejected and why.
4. Read **Impact** to understand which spec documents and Phase 7 tasks are affected.

Mark each with one of:

- `[ ] AGREED` — accept as written; implementation may proceed.
- `[ ] REJECTED` — do not implement; provide a counter-statement for the open-items list.
- `[ ] MODIFIED` — accept with changes; write the modification inline before merging.

**Lock sequence:**

1. All P0 items must be AGREED or REJECTED before Phase 7 T1 begins.
2. All P1 items must be AGREED or REJECTED before Phase 7 T3 begins.
3. P2 items are reviewed before the 0.1.0 release milestone.

Once all P0 items are locked:

- Update `docs/spec/migrations.md` per the spec-update manifest in Part V.
- Add new rows to `docs/spec/decisions.md` for each locked decision.
- Update the Phase 7 v2 plan DDL to the finalized ledger shape.

---

## Part I: P0 Recommendations (Must-Have Before Phase 7 Coding Begins)

---

### R-01: Migration runner is Djogi-owned, not `sqlx::migrate`

**Priority:** P0

**Recommendation:** The Djogi migration runner MUST NOT use `sqlx::migrate!` or any
`sqlx` migration execution machinery. It MUST be implemented directly on `tokio-postgres 0.7`
and `deadpool-postgres 0.14` (the stack already in place since Phase 5-Zero). The runner is
entirely Djogi-owned: planner, SQL emitter, ledger, advisory lock, and snapshot update all live
in `djogi/src/migrate/runner.rs` and adjacent modules. No `sqlx` migration API surface may
survive into the Phase 7 system.

**Rationale:** `topics/12-rust-ecosystem-contrast.md` documents that `sqlx::migrate` uses a
minimalist two-column ledger (`_sqlx_migrations`: `version`, `applied_on`) with no checksum
column, no status column, no run_id, and no execution_mode. This schema is structurally
incapable of supporting Djogi's ledger requirements (G-03). `topics/04-advisory-locks-and-concurrency.md`
confirms that `sqlx::migrate` does not use advisory locking — a concurrent apply will corrupt
the ledger silently. `topics/05-transactional-vs-non-transactional.md` establishes that
`sqlx::migrate` wraps every DDL statement in a single transaction without non-transactional
segment awareness, which would cause `CREATE INDEX CONCURRENTLY` to fail at the Postgres level
with an error inside a transaction block. Phase 7 design §Runner Ownership and Phase 7 v2
plan §Critical Design Decision 1 both state explicitly: "No `sqlx::migrate` compatibility layer
survives into the real migration system." `docs/spec/migrations.md` §10.1 has not been updated
to reflect this decision.

**Resolves:** C-01 (SPEC-M §10.1 says "Execution is sqlx's built-in runner" — directly
contradicted by P7D §Runner Ownership and P7V2 §Critical Design Decision 1).

**Alternatives considered:**
(a) Keep `sqlx::migrate` for v0.1 as a temporary scaffold and replace it in v0.2. Rejected:
the ledger DDL is a breaking change between `_sqlx_migrations` and `djogi_schema_migrations`,
and migrating existing ledger rows would require a bootstrapping migration that itself needs
a runner. Starting from the Djogi-owned runner avoids this trap.
(b) Wrap `sqlx::migrate` with a thin compatibility shim. Rejected: the advisory lock and
non-transactional segment gaps cannot be fixed by a shim without re-implementing the runner.

**Impact:** Update `docs/spec/migrations.md` §10.1 to remove "sqlx's built-in runner" and
"tracked in `_sqlx_migrations`". Lock in `docs/spec/decisions.md` as a new decision row.
Phase 7 file structure adopts `djogi/src/migrate/runner.rs` as the implementation home.

---

### R-02: Ledger table name is `djogi_schema_migrations`

**Priority:** P0

**Recommendation:** The Djogi migration ledger table MUST be named `djogi_schema_migrations`.
This name MUST appear in every spec document, code comment, test fixture, and CLI output that
references the ledger. The name `_sqlx_migrations` MUST NOT appear in any Djogi documentation,
code, or test. Every occurrence of `_sqlx_migrations` in the repository is a C-02 defect that
must be resolved before Phase 7 T1 begins.

**Rationale:** `docs/spec/migrations.md` §10.1 implies `_sqlx_migrations` by referencing
sqlx's built-in runner (C-01). The local Phase 7 migration-system v2 implementation plan
§Ledger shape uses `djogi_schema_migrations` in the DDL. These two documents conflict on the
name (C-02). The Phase 7 v2 plan is authoritative. The name `djogi_schema_migrations` follows
the pattern of other framework-owned tables (`djogi__crud_log`, `djogi__events`), is
human-recognizable in `pg_tables` output, and does not collide with any known third-party table
name convention (Flyway: `flyway_schema_history`; Liquibase: `databasechangelog`; Prisma:
`_prisma_migrations`; Diesel: `__diesel_schema_migrations`; Alembic: `alembic_version`).

**Resolves:** C-02 (implied `_sqlx_migrations` vs. explicit `djogi_schema_migrations`).

**Alternatives considered:**
(a) `_djogi_migrations` with a leading underscore (Prisma-style "private" convention). Rejected:
Djogi does not use a leading-underscore convention for any other framework table. Consistency
with Djogi's existing naming is more important than copying Prisma's convention.
(b) `djogi_migrations` (without `_schema_`). Rejected: the longer name is unambiguous when
`pg_tables` lists both user tables and framework tables. Operators searching for migration state
should not have to disambiguate from a user-created `djogi_migrations` table.

**Impact:** Audit all Djogi documentation for `_sqlx_migrations`. Update `docs/spec/migrations.md`
§10.1. Lock in `docs/spec/decisions.md`.

---

### R-03: Advisory lock key is `0x444A4F474D494752` (decimal: 4994068948568834898)

**Priority:** P0

**Recommendation:** The Djogi migration runner MUST acquire a Postgres session-scoped advisory
lock using the key `0x444A4F474D494752` (the ASCII bytes of `DJOGMIGR` packed into a 64-bit
integer, decimal value `4994068948568834898`). The acquire call MUST use
`pg_try_advisory_lock(4994068948568834898)` in a retry loop with a configurable wait timeout
(default 30 seconds, configurable via `Djogi.toml [migrations] lock_timeout_secs`). The release
call MUST use `pg_advisory_unlock(4994068948568834898)` in a `finally`-equivalent cleanup path
that runs on success, failure, and process signal. This key MUST be recorded as a locked constant
in `docs/spec/decisions.md` so future Djogi tooling does not introduce a second distinct lock key.

The collision analysis:
- Prisma's key: `72707369` — differs by ~5 trillion.
- Flyway's range: approximately `0x466C797761790000` to `0x466C797761790000 + 2^32 - 1`
 (the ASCII bytes of "Flyway" shifted to the upper six bytes, plus a 32-bit hash discriminator
 in the lower four bytes). The Djogi key `0x444A4F474D494752` begins with `0x44` ("D") while
 Flyway's range begins with `0x46` ("F"), making overlap structurally impossible.
- No other surveyed system uses advisory locking in Rust, so there are no additional collision
 risks in the Rust ecosystem at this time.

**Rationale:** `topics/04-advisory-locks-and-concurrency.md` §Key derivation strategies confirms
the Djogi candidate key `x'DJOGMIGR'::bigint` = `4994068948568834898` is distinct from Prisma's
`72707369` and outside Flyway's range. It also establishes that Prisma's hardcoded magic constant
approach — with no derivation rationale — is a collision risk for shared infrastructure. The
gap analysis document (G-01) flags this as a PRIORITY-1 item: "Djogi must not collide with
Prisma's hardcoded `72707369`." No spec document currently specifies Djogi's advisory lock key.

**Resolves:** G-01.

**Alternatives considered:**
(a) Derive the lock key from a SHA-256 hash of the schema name (so different Postgres schemas
get different keys). Rejected: schema-name-derived keys mean Djogi's lock key changes if the
schema is renamed, producing a stale lock that does not block the renamed runner. The simpler
fixed-constant approach is sufficient for a Postgres-only, single-schema system.
(b) Use Flyway's approach of `magic + hashCode(table)`. Rejected: Java's `String.hashCode()`
is not reproducible from Rust without re-implementing the algorithm. A stable constant is
simpler and equally correct.
(c) Use a transaction-scoped lock (`pg_try_advisory_xact_lock`) instead of session-scoped.
Rejected: transaction-scoped locks release at COMMIT/ROLLBACK, meaning the lock is released
before snapshot update completes (snapshot update is a file I/O operation, not a DB transaction).
Session-scoped locks protect the full runner lifecycle.

**Impact:** Add `lock_timeout_secs` to `Djogi.toml` migration section. Add the key value as a
constant in `djogi/src/migrate/runner.rs`. Lock in `docs/spec/decisions.md`.

---

### R-04: Ledger DDL is the complete finalized version (5 additions to P7V2 draft)

**Priority:** P0

**Recommendation:** The `djogi_schema_migrations` ledger DDL MUST use the following finalized
schema. This supersedes the draft in the local Phase 7 migration-system v2 implementation plan
§Ledger shape:

```sql
CREATE TABLE IF NOT EXISTS djogi_schema_migrations (
  -- Surrogate primary key (stable across multiple rows per version if failure rows exist)
  id        BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,

  -- Natural version key — e.g. "0001_create_users"
  version     TEXT     NOT NULL UNIQUE,
  description   TEXT     NOT NULL DEFAULT '',

  -- Checksums: SHA-256 hex with V1: prefix (see R-05)
  checksum_up   VARCHAR(68)  NOT NULL,      -- "V1:" + 64 hex chars
  checksum_down  VARCHAR(68),           -- NULL only if no _down.sql paired file

  -- Execution tracking
  execution_mode  TEXT     NOT NULL DEFAULT 'transactional'
             CHECK (execution_mode IN ('transactional', 'non_transactional')),

  -- Status: explicit state column — Prisma pre-write row pattern (see R-06)
  status      TEXT     NOT NULL DEFAULT 'pending'
             CHECK (status IN ('pending', 'applied', 'failed', 'rolled_back')),

  -- Timestamps
  applied_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),

  -- Attribution — populated via SELECT current_user at migration time
  applied_by    TEXT     NOT NULL DEFAULT current_user,

  -- Performance
  execution_time_ms BIGINT    NOT NULL DEFAULT 0,

  -- Out-of-order flag (see R-09)
  out_of_order_flag BOOLEAN   NOT NULL DEFAULT false,

  -- Partial-apply state for non-transactional segments (see R-07)
  partial_apply_state  TEXT,
  partial_apply_detail TEXT,

  -- Deployment group — groups all migrations from one cargo djogi migrate invocation
  run_id      TEXT,

  -- Snapshot version this migration was applied against
  snapshot_version TEXT     NOT NULL
);

-- Fast lookup for anything that is not cleanly applied
CREATE INDEX djogi_schema_migrations_status_idx
  ON djogi_schema_migrations (version)
  WHERE status != 'applied';

-- Fast lookup by deployment run
CREATE INDEX djogi_schema_migrations_run_id_idx
  ON djogi_schema_migrations (run_id)
  WHERE run_id IS NOT NULL;
```

The five changes from the P7V2 draft are:
1. Added `id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY` (surrogate PK) alongside
  `version TEXT NOT NULL UNIQUE` (natural key). Required to allow multiple rows per version
  when failure rows are retained as audit trail.
2. Added `status TEXT NOT NULL DEFAULT 'pending' CHECK (...)` with the pending pre-write
  pattern. This eliminates the crash-window where DDL completes but ledger write does not.
3. Added `applied_by TEXT NOT NULL DEFAULT current_user`. Populated via `SELECT current_user`
  at migration time — not from environment variables.
4. Added `run_id TEXT` to group all migrations from one `cargo djogi migrate` invocation.
5. Added `execution_time_ms BIGINT NOT NULL DEFAULT 0`.

**Rationale:** `topics/02-ledger-schema.md` §Column-by-column analysis validates each addition:
(1) surrogate PK is required if failure rows persist (Flyway uses `installed_rank` for this);
(2) `status = 'pending'` pre-write eliminates the crash window that affects refinery, cot, and
Liquibase; (3) `applied_by` is Flyway's `installed_by` pattern, valued for audit and
post-incident investigation; (4) `run_id` is Liquibase's `DEPLOYMENT_ID` concept, unique to
Liquibase among surveyed systems and explicitly called out as a gap-worth-filling in T02;
(5) `execution_time_ms` matches Flyway's `execution_time` column, useful for migration
performance monitoring. The `source_checksum` column (which appeared in some earlier Djogi
drafts) is explicitly dropped per T02 §Column to drop — it is a build-time artifact, not a
runtime ledger concern.

**Resolves:** G-03 (ledger DDL incomplete).

**Alternatives considered:**
(a) Keep `version TEXT PRIMARY KEY` and drop the surrogate PK. Rejected: if Djogi ever retains
failure rows in the ledger (R-06 requires this), a natural PK on `version` prevents inserting
a second row for the same migration. Flyway made this exact mistake and introduced `installed_rank`
to fix it.
(b) Use a UUID for `id` (Prisma's pattern). Rejected: a BIGINT identity is simpler, sortable,
and consistent with HeerId's BIGINT-first philosophy.
(c) Omit `run_id`. Rejected: production post-mortems ("what changed in yesterday's deploy?")
are substantially easier with a deployment-group column. No other Rust migration system has this,
making it a genuine Djogi differentiator.

**Impact:** Replaces P7V2 §Ledger shape DDL. Update `docs/spec/migrations.md` to include the
finalized DDL in a new §10.7. Phase 7 T2 (ledger implementation) implements this schema.

---

### R-05: Checksum algorithm is SHA-256 over UTF-8 SQL bytes; format is `V1:<64 hex chars>`

**Priority:** P0

**Recommendation:** Djogi MUST compute migration checksums as follows:

- **Algorithm:** SHA-256 (from the `sha2` Rust crate, the same implementation Prisma uses).
- **Input:** The raw UTF-8 bytes of the SQL file after the following normalization:
 - Strip a leading BOM (`EF BB BF`) if present.
 - Normalize all line endings to `\n` (replace `\r\n` with `\n`, replace bare `\r` with `\n`).
 - No trailing whitespace stripping. No comment stripping.
- **What NOT to hash:** The migration filename, the version number, or the description. Only
 the SQL content is hashed. This is the opposite of refinery's design (refinery hashes
 `name + version + sql`), which causes checksum changes on file renames even when SQL is
 unchanged.
- **Storage format:** `V1:` prefix followed by the 64-character lowercase hex digest, for a
 total stored width of 67 characters. The column type is `VARCHAR(68)` to accommodate any
 future single-character prefix increment.
- **Algorithm versioning:** When the algorithm is upgraded from SHA-256 to a future hash,
 increment the version prefix to `V2:`, `V3:`, etc. Existing `V1:` rows remain valid and
 are compared using the V1 algorithm. This follows Liquibase's `V:hex` versioned format
 (`liquibase-standard/src/main/java/liquibase/change/CheckSum.java:85-91` and
 `ChecksumVersion.java:14-22`).
- **Both files are checksummed:** `checksum_up` covers the `_up.sql` file. `checksum_down`
 covers the `_down.sql` file (nullable only when no `_down.sql` exists, not when the file
 is empty — an empty file receives a checksum of the empty string after normalization).
- **Repair semantics:** After a human edits an applied migration file with documented reason,
 `cargo djogi migrate repair` re-reads the file, recomputes the checksum, and updates the
 ledger row with operator confirmation. The old checksum and new checksum are both shown
 during confirmation.

**Rationale:** `topics/03-checksums-and-repair.md` §Algorithm landscape establishes the
hierarchy: SHA-256 (Prisma) > MD5 with `V:hex` prefix (Liquibase) > CRC-32 (Flyway) >
SipHash-1-3 (refinery). The verbatim Prisma source at
`prisma-engines-reference/schema-engine/connectors/schema-connector/src/checksum.rs:43-48`
confirms SHA-256 via `sha2::Sha256`. Refinery's anti-pattern is documented at
`refinery_core/src/runner.rs:92-96` — hashing name+version+sql means a file rename changes
the checksum. Liquibase's `V:hex` versioned prefix format is the adopt-worthy pattern for
future-proofing. `topics/01-source-of-truth-and-state.md` §Adopt confirms: "The format should
include a version prefix (`V1:...`) following Liquibase's pattern."

**Resolves:** G-02 (checksum algorithm, normalization, and version prefix unspecified).

**Alternatives considered:**
(a) CRC-32 (Flyway's algorithm). Rejected: 32-bit collision space is non-negligible at scale;
signed-integer storage is awkward in Rust/Postgres. SHA-256's 256-bit space is effectively
collision-free.
(b) SipHash-1-3 (refinery). Rejected: not cryptographic; the name+version composite hashing
is a known anti-pattern that breaks checksum continuity on file renames.
(c) Unversioned hex string. Rejected: algorithm upgrades would require a schema migration of
the ledger table and invalidate historical rows.
(d) Hash comments and whitespace as part of the content. Rejected: minor formatting changes
(adding a blank line, reformatting a comment) should not trigger checksum drift. The
normalization rules above match Prisma's practical approach (bidirectional line-ending
normalization without stripping).

**Impact:** Phase 7 T2 (ledger) and T3 (checksum verification in runner) implement this.
`docs/spec/migrations.md` §10.1 gains a "Checksum" subsection with the exact algorithm and
format. Lock in `docs/spec/decisions.md`.

---

### R-06: Ledger INSERT uses the pending pre-write row pattern

**Priority:** P0

**Recommendation:** Before executing any DDL in a migration, the runner MUST INSERT a row
into `djogi_schema_migrations` with `status = 'pending'`. After successful DDL execution,
the runner MUST UPDATE the same row to `status = 'applied'` and populate `applied_at`,
`execution_time_ms`, and `applied_by`. On DDL failure, the runner MUST UPDATE the row to
`status = 'failed'` and set `partial_apply_detail` for non-transactional segments.

The INSERT and the DDL MUST be inside the same database transaction for transactional
migrations (the INSERT rolls back atomically with the DDL on failure, leaving no orphan
`pending` row). For non-transactional migrations the INSERT is committed before DDL begins
(because the DDL cannot be inside a transaction), leaving the `pending` row visible as an
explicit "migration in flight" marker.

On the next `cargo djogi migrate` invocation after a crash, the runner MUST detect any
`pending` rows and halt with a diagnostic before attempting further migrations:

```
error[M007]: migration "0009_add_payment_index" has status 'pending'
 = help: a previous run may have crashed mid-migration
 = help: run `cargo djogi migrate repair` to resolve
```

**Rationale:** `topics/02-ledger-schema.md` §Status and failure flags establishes that Prisma
is the only surveyed system with a first-class "migration in flight" state via nullable
`finished_at`. All other systems have binary applied/not-applied state, which creates an
undetectable crash window: DDL commits, process dies, ledger row never written. The explicit
`status = 'pending'` pre-write pattern is T02's primary recommendation for eliminating this
window. `topics/05-transactional-vs-non-transactional.md` confirms the two-phase commit
requirement for non-transactional migrations — the INSERT must commit before DDL begins.

**Resolves:** Closes the partial-apply crash window (part of G-03; reinforces G-06).

**Alternatives considered:**
(a) Write the ledger row only after DDL succeeds (all other systems except Prisma). Rejected:
this leaves a detectable gap where DDL has committed but the process dies before the INSERT.
The next migrator sees a clean ledger and re-runs the migration, causing duplicate DDL or
constraint violations.
(b) Write `finished_at IS NULL` to track in-flight state (Prisma's exact approach). Rejected:
an explicit `status` column is cleaner than inferring state from nullable timestamps. The
CHECK constraint on `status` also makes invalid states impossible to INSERT.

**Impact:** Runner implementation in Phase 7 T5. `docs/spec/migrations.md` §10.7 documents
the pre-write pattern and the pending-detection behavior.

---

### R-07: Partial non-transactional failure writes a `migrations/.migration_failure.json` marker

**Priority:** P0

**Recommendation:** When a non-transactional migration segment fails partway through (DDL has
committed for statements 1..N but statement N+1 fails), the runner MUST:

1. Update the ledger row to `status = 'failed'`, `partial_apply_state = 'segment_N_of_M'`,
  `partial_apply_detail = '<statement text>'`.
2. Write a marker file at `migrations/.migration_failure.json` containing:
  ```json
  {
   "failed_version": "0009_add_payment_index",
   "failed_segment": 2,
   "failed_at": "2026-04-22T10:30:00Z",
   "expected_next_snapshot_version": "0009"
  }
  ```
3. Refuse to plan or apply further migrations until the marker is cleared by
  `cargo djogi migrate repair`.

The marker file, not just the ledger row, is the blocking signal because the runner cannot
connect to the database to read the ledger until a connection is available. The marker is a
fast-fail gate that protects the snapshot invariant without a DB round-trip.

`cargo djogi migrate repair` removes the marker file, prompts the operator for confirmation
of the partial-apply state, and transitions the ledger row to `status = 'applied'` (if the
operator confirms the partial apply is complete and safe) or `status = 'rolled_back'` (if
the operator rolled back manually).

**Rationale:** `topics/01-source-of-truth-and-state.md` §Open design gap explicitly
identifies the need for a `migration_failure.json` marker: "on any migration failure, the
runner must write a marker so the next invocation knows it is starting from a potentially
inconsistent state." `topics/05-transactional-vs-non-transactional.md` establishes that for
non-transactional DDL there is no rollback guarantee. The gap analysis (G-06) names this the
snapshot-side invariant problem: the ledger and snapshot agree something failed, but without
the marker, the runner cannot distinguish a clean start from a post-failure resume.

**Resolves:** G-06 (snapshot invariant under partial non-transactional failure).

**Alternatives considered:**
(a) Use only the ledger `status = 'failed'` row as the blocking signal. Rejected: the runner
needs a DB connection to read the ledger. If the failure left the DB in an unusual state
(e.g., the connection pool is exhausted), the runner cannot safely proceed even to read
the ledger.
(b) Use a lock file in `migrations/` (always present, never deleted on success). Rejected:
a lock file that is always present creates confusion between "migration in progress" and
"migration failed." The marker file only exists when there is a problem to resolve.
(c) Embed the failure state in `schema_snapshot.json`. Rejected: the snapshot must represent
a clean applied schema state or be absent. Corrupting the snapshot format to track failure
state undermines the core invariant.

**Impact:** Runner (Phase 7 T5), repair command (Phase 7 T6), CLI diagnostic output.
`docs/spec/migrations.md` §10.3 documents the snapshot invariant and the marker file protocol.

---

### R-08: `-- djogi:no-transaction` directive specification

**Priority:** P0

**Recommendation:** The `-- djogi:no-transaction` directive MUST conform to the following rules:

- **Placement:** The directive MUST appear on the first non-blank, non-comment line of the
 SQL file. Placement on any other line causes a parse error, not silent acceptance.
 Correct: `-- djogi:no-transaction` appears before any SQL statements.
- **Scope:** When present, the directive causes the runner to treat the entire file as a
 single non-transactional segment. No transaction wrapper is added for any statement in
 the file. Segment planning does not override this directive.
- **Direction independence:** The directive MAY appear in the `_up.sql` file without
 appearing in the `_down.sql` file and vice versa. Each file's directive is evaluated
 independently.
- **Relationship to auto-detection:** Djogi derives execution mode from structured operation
 kinds and descriptor metadata (per P7D §Transaction Boundary Decision). The directive
 supplements this detection — it is the override for cases where the auto-detection is
 wrong or where a human-written file contains statements the auto-detector cannot classify.
- **Generated headers:** When Djogi generates a migration file that includes non-transactional
 operations (e.g., `CREATE INDEX CONCURRENTLY`), the generator MUST emit the directive
 automatically and set `Execution-Mode: non_transactional` in the file header comment.
 Human-authored override files that need non-transactional execution MUST add the directive
 manually.
- **Error surface:** If the runner encounters a statement that Postgres will reject inside a
 transaction (e.g., `CREATE INDEX CONCURRENTLY`) in a file without the directive, it MUST
 emit an error before attempting execution:
 ```
 error[M012]: migration "0007_add_payment_index" contains non-transactional statement
  --> migrations/0007_add_payment_index_up.sql:8
  = help: add `-- djogi:no-transaction` as the first line of this file
 ```

**Rationale:** `topics/05-transactional-vs-non-transactional.md` §Approaches establishes that
the directive-based opt-out is the correct mechanism (used by Diesel via `metadata.toml`,
Django via `atomic = False`, and implied in the Djogi design). The local Phase 7 migration-system design notes §Transaction Boundary Decision confirms the structured-detection-first
approach. The local Phase 7 migration-system v2 implementation plan §Generated file
headers shows `-- Execution-Mode: transactional` in generated files but does not yet define
the `-- djogi:no-transaction` override directive syntax. The gap analysis (G-05) identifies
this as unspecified in both SPEC-M and P7V2.

**Resolves:** G-05 (`-- djogi:no-transaction` directive unspecified).

**Alternatives considered:**
(a) Flyway's auto-detection via keyword scanning. Rejected per P7D: "string scanning must not
be the primary design." The no-regex rule (SPEC-D) also prohibits keyword-list detection.
(b) A separate `metadata.toml` sidecar file (Diesel's approach). Rejected: paired SQL files
are Djogi's primary artifact; adding a third file per migration increases complexity with no
benefit over a one-line directive in the SQL file itself.
(c) Allow the directive anywhere in the file. Rejected: requiring first-line placement makes
parsing deterministic without scanning the entire file and makes the file's execution mode
visible at a glance.

**Impact:** SQL parser (Phase 7 T3), segment planner (Phase 7 T3), SQL generator (Phase 7 T4).
Add directive specification to `docs/spec/migrations.md` §10.4.

---

### R-09: Out-of-order policy is environment-sensitive with mandatory ledger flagging

**Priority:** P0

**Recommendation:** Djogi MUST implement the following out-of-order migration policy:

- **Local/dev environment** (detected via `dev_mode = true` in `Djogi.toml` or
 `DJOGI_ENV=dev`): out-of-order migrations are ALLOWED by default. An out-of-order
 migration is any migration whose version is lower than the highest already-applied version.
 The apply proceeds, but the ledger row MUST have `out_of_order_flag = true`. `cargo djogi plan`
 MUST surface this flag with a prominent warning.
- **CI/prod environment** (any environment where `dev_mode` is absent or false): out-of-order
 migrations are REJECTED by default with a hard error before any DDL executes.
- **Override:** `cargo djogi migrate --allow-out-of-order` overrides the rejection in any
 environment and records the apply with `out_of_order_flag = true`. This flag is also
 available on `cargo djogi makemigrations`.
- **Visibility:** `cargo djogi migrate show` MUST display all rows with `out_of_order_flag = true`
 with a visual indicator, so operators can audit historical out-of-order applies.
- **Out-of-order is never silent.** Whether allowed or rejected, the runner MUST emit a
 diagnostic that names the out-of-order migration and the migration it arrived after.

**Rationale:** `topics/06-out-of-order-and-baseline.md` §The out-of-order problem establishes
the scenario: two developers branch, both write migrations with adjacent version numbers, one
merges first and is deployed. The merged-second migration has a lower version number than what
was already applied. The local Phase 7 migration-system design notes
§Out-of-Order Policy states: "local/dev: allow by default, but record and warn loudly;
CI/prod: reject by default." This is the correct tiered policy. No surveyed Rust system
implements this — Djogi would be the only one.

**Resolves:** Part of G-17 (build-time policy); I-24 (out-of-order policy confirmed as
environment-sensitive). Validates the P7D design choice.

**Alternatives considered:**
(a) Reject out-of-order always. Rejected: normal branch-based development in a team produces
out-of-order migrations routinely. Strict rejection everywhere would require developers to
constantly resequence migration numbers, adding friction with no safety benefit in development.
(b) Allow out-of-order silently in all environments (Django's behavior). Rejected: silent
allowance in production is how schema states diverge without anyone noticing. The ledger flag
and `show` output visibility are the minimum safety net.
(c) Use Alembic's branch/head model. Rejected: Alembic's DAG-based approach is powerful but
requires developers to declare branch heads explicitly, which conflicts with Djogi's sequential
`NNNN_name` scheme and the submodule-managed migrations folder.

**Impact:** Runner (Phase 7 T5), CLI plan/show/migrate (Phase 7 T6), config parsing (T2).
Lock in `docs/spec/decisions.md`. `docs/spec/migrations.md` §10.2 documents the policy tiers.

---

### R-10: Baseline and fake are first-class flows with defined semantics

**Priority:** P0

**Recommendation:** Djogi MUST ship the following adoption-flow commands before v0.1.0:

- **`cargo djogi migrate baseline <version>`:** Inserts ledger rows with `status = 'applied'`
 for all migrations up to and including `<version>` without executing any SQL. The DDL for
 those migrations is assumed to already exist in the database. The snapshot is set to `<version>`.
 This is for adopting Djogi on an existing database.
- **`cargo djogi migrate --fake <version>`:** Inserts a single ledger row with `status = 'applied'`
 for exactly the named migration without executing any SQL. For use when a migration has been
 applied manually or by another tool.

Both commands MUST:
- Acquire the advisory lock before writing any ledger rows (same lock as `cargo djogi migrate`).
- Set `applied_by = current_user`, `applied_at = now()`, `checksum_up = <computed from file>`,
 `execution_time_ms = 0`.
- NOT write a `pending` row first (there is no DDL to protect against a crash window).
- Advance the snapshot to reflect the baseline/faked version.

The distinction between `baseline` (bulk floor-setting) and `--fake` (single-migration stamp)
follows Flyway's semantics as documented in `topics/06-out-of-order-and-baseline.md`.

**Rationale:** `topics/06-out-of-order-and-baseline.md` §Comparison matrix: baseline / fake /
stamp documents that all systems with any adoption story converge on a per-migration or
floor-setting stamp primitive. The local Phase 7 migration-system design notes
§Baseline Adoption states: "Baseline adoption is mandatory. Adoption of an existing database
must not require manual inserts into the ledger table." I-25 confirms this as validated.

**Resolves:** Validates I-25 and closes the adoption-workflow gap (no specific gap ID assigned,
but the Phase 7 design doc marks this as required).

**Alternatives considered:**
(a) Merge `baseline` and `--fake` into one command. Rejected: the semantic difference matters.
`baseline` implies "the database schema predates this migration system and is assumed to be at
version N." `--fake` implies "this specific migration was applied by some other means and
should be recorded." Conflating them makes adoption workflows harder to explain.
(b) Require manual INSERT into the ledger. Rejected explicitly by P7D §Baseline Adoption.

**Impact:** Runner (Phase 7 T5, repair/baseline subflow). CLI (Phase 7 T6). `docs/spec/migrations.md`
new §10.8 (adoption flows).

---

### R-11: `SchemaDelta` enum extended to the complete Phase 7 surface

**Priority:** P0

**Recommendation:** The `SchemaDelta` enum in `docs/spec/migrations.md` §10.6 MUST be updated
to the following complete Phase 7 variant list. The current draft enum is an early sketch that
is materially incomplete and will cause an under-featured differ if implemented as written:

```rust
// COMPLETE Phase 7 SchemaDelta — supersedes docs/spec/migrations.md §10.6
enum SchemaDelta {
  // Tables
  CreateTable { table: TableDef },
  DropTable { name: String },
  RenameTable { old_name: String, new_name: String },

  // Columns
  AddColumn { table: String, column: ColumnDef },
  DropColumn { table: String, name: String },
  AlterColumn { table: String, name: String, change: ColumnChange },
  RenameColumn { table: String, old_name: String, new_name: String },

  // Unique constraints (constraint form, not index form — see R-17)
  AddUniqueConstraint { table: String, constraint: UniqueConstraintDef },
  DropUniqueConstraint { table: String, name: String },

  // Indexes
  AddIndex { table: String, index: IndexDef },
  DropIndex { name: String },

  // Foreign keys
  AddForeignKey { table: String, fk: ForeignKeyDef },
  DropForeignKey { table: String, name: String },

  // Enums
  CreateEnum { name: String, variants: Vec<String> },
  AlterEnum { name: String, change: EnumChange },
  DropEnum { name: String },

  // Extensions
  CreateExtension { name: String, version: Option<String> },
  DropExtension { name: String },
}
```

**Rationale:** the local Phase 7 migration-system v2 implementation plan §Canonical Scope
lists the full differ surface: enum types, extensions, partition metadata, explicit rename operations
(`RenameColumn`, `RenameTable`), composite unique constraints, and JSONB index metadata. The current
`SchemaDelta` enum in SPEC-M §10.6 lacks `RenameColumn`, `RenameTable`, `CreateEnum`, `AlterEnum`,
`DropEnum`, `CreateExtension`, `DropExtension`, and `AddUniqueConstraint`/`DropUniqueConstraint`.
This is contradiction C-03: the enum implies completeness it does not have.

**Resolves:** C-03 (`SchemaDelta` enum incomplete vs. P7V2 scope).

**Alternatives considered:**
(a) Mark SPEC-M §10.6 as a sketch and let the Phase 7 implementation define the final enum.
Rejected: the enum is the normative contract for the differ — any implementer who reads SPEC-M
will build an under-featured differ. The spec must reflect the intended surface.
(b) Add partition-specific variants now. Rejected: partition DDL support is in scope for Phase 7
but the exact variant shape depends on the partition metadata design (a separate sub-task).
Partition variants are deferred to the Phase 7 T1 implementation where the internal schema model
defines the partition descriptor shape.

**Impact:** Rewrite SPEC-M §10.6 with the above enum. Differ implementation (Phase 7 T1/T2).

---

### R-12: `build.rs` emits diagnostics only; migration file generation requires `cargo djogi makemigrations`

**Priority:** P0

**Recommendation:** `build.rs` MUST NOT write migration SQL files to the `migrations/` directory.
`build.rs` MUST ONLY emit compiler diagnostics (warnings) when drift is detected between
`target/djogi_models.json` and `migrations/schema_snapshot.json`. The developer MUST run
`cargo djogi makemigrations` to generate the actual migration SQL files.

The warning diagnostic emitted by `build.rs` MUST change to reflect this:
```
warning[D001]: schema drift detected — run `cargo djogi makemigrations` to generate migration
 --> src/apps/vehicles/models.rs:8:9
  |
 8 |   pub horsepower: i32,
  |   ^^^^^^^^^^^^^^^^^^^ new field — no migration generated yet
  |
  = help: run `cargo djogi makemigrations` when ready
  = help: or run `cargo djogi makemigrations --dry-run` to preview
```

`docs/spec/migrations.md` §10.2 currently says "Generates a migration pair... into `migrations/`"
(implying file-write from `build.rs`). `docs/spec/decisions.md` row "Migration generation" says
"Automatic via `build.rs` on drift detection — generates pair". Both of these statements MUST
be updated to "detects drift and emits diagnostic — file generation requires `makemigrations`."

**Rationale:** `topics/12-rust-ecosystem-contrast.md` §build.rs IDE-churn risk identifies
that `build.rs` writing files to `migrations/` on every `cargo build` causes editors to
re-read the directory on every build, triggering unnecessary file watches and LSP re-indexing.
More importantly, the local Phase 7 migration-system design notes §Ledger
and Snapshot Model states: "`build.rs` may read the snapshot. It must never mutate it." Mutating
the snapshot is the action that should be gated; but file-write from `build.rs` also produces
intermediate migration files that may exist transiently without being reviewed. The separation
of "detect drift" (build.rs) from "generate files" (makemigrations) is cleaner. Gap G-17 flags
this as a contradiction between SPEC-M §10.2 and P7D §Core Model. The `migrations/` folder is
a git submodule (SPEC-D); `build.rs` should not write to a submodule without developer review.

**Resolves:** G-17 (`build.rs` file-write vs. warning-only distinction).

**CONTRADICTS locked decisions §Build drift diagnostic and §Migration generation** — both
decisions describe automatic file-write from `build.rs`. This recommendation proposes changing
those decisions to diagnostic-only. **This requires user re-open of those two locked decisions.**

**Alternatives considered:**
(a) Keep file-write from `build.rs`. Rejected because it causes IDE churn, writes to the git
submodule without developer action, and conflicts with P7D's "never mutate the snapshot" principle
(which, extended consistently, implies `build.rs` should not be the mutation point for migration
files either).
(b) Write files to `target/` (not `migrations/`) from `build.rs`, and copy them to `migrations/`
only when the developer confirms. Rejected: this two-copy workflow is confusing and adds a
`target/` state that must be reconciled with the submodule.

**Impact:** Update SPEC-M §10.2 and SPEC-D rows "Build drift diagnostic" and "Migration
generation." `build.rs` implementation (Phase 7 T8). `makemigrations` CLI (Phase 7 T7).

---

### R-13: Rollback ordering is by `installed_rank` (temporal), not by version number

**Priority:** P0

**Recommendation:** `cargo djogi migrate rollback` MUST undo migrations in reverse order of
their `id` column (which is a BIGINT identity that increases monotonically with each INSERT).
This is temporal order — the migration most recently applied is rolled back first — not version
string order.

For example, if the ledger contains:
```
id=10 version=0009 applied_at=10:00 out_of_order_flag=false
id=11 version=0008 applied_at=10:05 out_of_order_flag=true
```
Then rollback MUST undo `id=11` (version 0008) first, then `id=10` (version 0009). Rolling
back version 0009 before 0008 would be wrong — 0008 was applied after 0009 and may depend
on schema state introduced by 0009.

The `-- djogi:rollback-order-note` comment SHOULD be present in generated down files when the
migration is at risk of out-of-order rollback (i.e., when `out_of_order_flag` is true in the
ledger).

**Rationale:** `topics/06-out-of-order-and-baseline.md` §Rollback ordering identifies this
explicitly: rollback must operate on temporal application order, not on version string order.
If 0008 was applied after 0009 (out-of-order), rolling back 0009 first would leave the database
in a state where 0008's `_down.sql` may reference objects 0009's `_up.sql` created. Version
string sort would produce the wrong rollback order. Gap G-07 is the formal citation.
the local Phase 7 migration-system design notes §Reversibility Decision
states rollback is supported but does not specify ordering — this recommendation closes that gap.

**Resolves:** G-07 (rollback ordering under out-of-order apply).

**Alternatives considered:**
(a) Rollback in reverse version string order. Rejected: this is wrong in the out-of-order case.
(b) Refuse rollback when out-of-order migrations are present. Rejected: too restrictive; developers
in branch-based workflows legitimately need to rollback after out-of-order applies.
(c) Use `applied_at` TIMESTAMPTZ for ordering. Rejected: timestamps can have sub-millisecond
collisions and are not guaranteed to be unique across rows. The `id` BIGINT identity is
strictly unique by construction.

**Impact:** Runner rollback flow (Phase 7 T5). Lock in `docs/spec/decisions.md`.
`docs/spec/migrations.md` §10.2 documents rollback ordering.

---

### R-14: Composite constraint and index attribute syntax is `#[model(indexes(...))]`

**Priority:** P0

**Recommendation:** Composite unique constraints and composite indexes MUST be declared via
a model-level attribute, not a field-level attribute, because they span multiple fields.
The adopted syntax is:

```rust
#[derive(Model)]
#[model(indexes(
  unique(fields = [col_a, col_b]),
  unique(fields = [col_c], name = "vehicles_vin_key"),
  index(fields = [col_d, col_e]),
  index(fields = [col_f], name = "vehicles_status_idx"),
))]
pub struct Vehicle {
  pub col_a: String,
  pub col_b: String,
  //...
}
```

Rules:
- `unique(fields = [...])` — generates `ALTER TABLE t ADD CONSTRAINT name UNIQUE (a, b)`
 (the constraint form, not `CREATE UNIQUE INDEX`, per R-17 rationale).
- `index(fields = [...])` — generates `CREATE INDEX name ON t (a, b)`.
- `name = "..."` is optional; when absent, the name is auto-generated per R-15.
- Column order within `fields = [...]` is preserved exactly in the generated SQL.
- Field-level `#[field(unique)]` and `#[field(index)]` remain valid and produce single-column
 constraints/indexes. They are NOT deprecated by this addition.

**Rationale:** `topics/08-composite-uniques-and-indexes.md` §Representation per system confirms
that model-level attribute syntax is the correct approach for composite constraints (Django's
`Meta.indexes`, SQLAlchemy's `UniqueConstraint` at the table level, Prisma's `@@unique`/`@@index`
annotations). Field-level attributes cannot span multiple fields by definition. Gap G-09
identifies this exact gap. The `#[model(indexes(...))]` form is consistent with the existing
`#[model(...)]` macro parameter style.

**Resolves:** G-09 (composite constraint attribute syntax unspecified).

**Alternatives considered:**
(a) A top-level `#[indexes(...)]` attribute separate from `#[model(...)]`. Rejected: two
separate attributes on the same struct increases macro complexity and can create ordering
ambiguity. The Djogi macro already uses `#[model(...)]` for all model-level metadata.
(b) A `#[derive(Model)]` with a separate `Indexes` derive macro. Rejected: this creates a
two-macro dependency that adds unnecessary complexity.
(c) Embed composite constraints in field attributes as `#[field(unique_with = [col_b])]`.
Rejected: this creates asymmetric ownership — the constraint would be "owned" by one field
but refer to another. Moving fields would silently orphan constraints.

**Impact:** `djogi-macros` (Phase 7 T1 or pre-T1). Update `docs/spec/models.md` with the
new attribute syntax.

---

### R-15: Composite constraint and index auto-generated names use `<table>_<col1>_<col2>_key` / `<table>_<col1>_<col2>_idx`

**Priority:** P0

**Recommendation:** When a composite constraint or index is declared without an explicit
`name = "..."` in `#[model(indexes(...))]`, Djogi MUST generate the name using the following
deterministic algorithm:

- **Unique constraint:** `<table>_<col1>_<col2>_..._key` (Postgres convention for unique
 constraints created via `ADD CONSTRAINT UNIQUE`).
- **Non-unique index:** `<table>_<col1>_<col2>_..._idx`.
- **Name length limit:** Postgres enforces a 63-byte limit on identifiers. If the generated
 name exceeds 63 bytes, apply the following truncation:
 1. Compute a SHA-256 hash of the full un-truncated name (ASCII bytes, no normalization needed
   since names are identifiers). Take the first 8 hex characters as a suffix.
 2. Truncate `<table>_<col1>_<col2>_..._` to fit within `63 - 1 - 8` = 54 bytes (leaving room
   for underscore + 8-char suffix).
 3. Final name: `<truncated_prefix>_<8hexchars>`.
- **Stability:** The algorithm is deterministic — the same table name and column list always
 produce the same name, regardless of the order in which other schema objects are declared.
 Reordering unrelated model fields or models does not change generated constraint names.
- **Column order:** The columns in the name appear in the same order as declared in `fields = [...]`.

**Rationale:** `topics/08-composite-uniques-and-indexes.md` §Naming convention documents this
exact algorithm, including the SHA-256 truncation approach. The Postgres 63-byte identifier
limit is enforced at the catalog level and cannot be bypassed. Non-deterministic names cause
unnecessary migration churn: if the name changes, the differ thinks the constraint was dropped
and re-created. Gap G-10 identifies this gap. The `<table>_<cols>_key` pattern follows Postgres's
own convention for auto-named unique constraints.

**Resolves:** G-10 (composite constraint naming and truncation algorithm unspecified).

**Alternatives considered:**
(a) Always require explicit names. Rejected: mandatory naming adds friction for the common case
and encourages copy-paste errors. Auto-generated names with a stable algorithm are the right default.
(b) Use MD5 for truncation (Django's approach). Rejected: Django uses an MD5-based hash but
documents it as "implementation detail, do not depend on it." SHA-256 is consistent with the
checksum algorithm (R-05) and has no collision concerns at 8 hex characters.
(c) Use sequential numbering for truncated names (`<table>_unique_001`). Rejected: sequential
names are not deterministic across schema changes — adding a new constraint can renumber existing
ones if they are stored in definition order.

**Impact:** `naming.rs` module (Phase 7 T1/T2). Update `docs/spec/migrations.md` with the
naming algorithm.

---

### R-16: `#[model(renamed_from = "old_table")]` is a locked annotation mirroring field-level rename

**Priority:** P0

**Recommendation:** Table renames MUST be declared via `#[model(renamed_from = "old_table")]`
on the model struct. This annotation MUST be added to `docs/spec/models.md` and
`docs/spec/decisions.md` as a locked annotation, mirroring the existing `#[field(renamed_from = "old_name")]`
decision already in SPEC-D.

The differ MUST treat `#[model(renamed_from = "old_table")]` as a `RenameTable` operation
(generating `ALTER TABLE old_table RENAME TO new_table`) rather than `DropTable { name: "old_table" } +
CreateTable { table: new_table }`. The annotation MUST be a migration-window-only marker
(removed after the rename migration is generated and applied) — see R-20 for the lifecycle
decision.

**Rationale:** `topics/07-rename-handling.md` establishes that heuristic rename detection is
unsafe (Prisma's rename heuristic is acknowledged as wrong by Prisma's own team). Explicit
annotation is Django's approach and the correct design. The local Phase 7 migration-system design notes
§Rename Decision and the local Phase 7 migration-system v2 implementation plan §Canonical
Scope both reference `#[model(renamed_from = "...")]`. Gap G-13 identifies that this annotation
appears in Phase 7 documents but is not yet in the base spec.

**Resolves:** G-13 (`#[model(renamed_from)]` not in base spec).

**Alternatives considered:**
(a) Heuristic detection by looking for a dropped model + a created model with the same field
set. Rejected explicitly in the Rename Decision section of P7D.
(b) A separate CLI command (`cargo djogi migrate rename-table old_table new_table`) that
auto-generates the rename migration. Rejected: this adds a special case to the CLI that
duplicates the descriptor-driven diff workflow. The annotation keeps all schema intent inside
the struct definition.

**Impact:** Update `docs/spec/models.md` and `docs/spec/decisions.md`. Differ implementation
(Phase 7 T1). `docs/spec/migrations.md` §10.6 (RenameTable variant already in R-11).

---

## Part II: P1 Recommendations (Should-Have for v0.1)

---

### R-17: Composite unique constraints use the `ADD CONSTRAINT UNIQUE` form, not `CREATE UNIQUE INDEX`

**Priority:** P1

**Recommendation:** When Djogi generates SQL for a composite unique constraint declared via
`#[model(indexes(unique(...)))]`, the generated SQL MUST use the constraint form:

```sql
ALTER TABLE vehicles ADD CONSTRAINT vehicles_vin_year_key UNIQUE (vin, year);
```

NOT the index form:

```sql
CREATE UNIQUE INDEX vehicles_vin_year_key ON vehicles (vin, year); -- DO NOT USE for unique constraints
```

The constraint form registers an entry in `pg_constraint` with `contype = 'u'`, enables
`ON CONFLICT ON CONSTRAINT vehicles_vin_year_key` upsert syntax, and allows the constraint
to be targeted by a foreign key. The index form does not.

`CREATE UNIQUE INDEX` (not `ADD CONSTRAINT UNIQUE`) remains correct for `#[field(index)]`
with `unique = true` that is an explicit performance index rather than a relational constraint.

**Rationale:** `topics/08-composite-uniques-and-indexes.md` §DB-level UNIQUE constraint vs
UNIQUE index documents the distinction. Prisma generates `CREATE UNIQUE INDEX` (missing the
constraint form). Django's `UniqueConstraint` generates `ALTER TABLE... ADD CONSTRAINT UNIQUE`
(correct). The distinction matters for `ON CONFLICT ON CONSTRAINT` syntax used in upsert
patterns. Since Djogi models business uniqueness invariants (not just performance indexes),
the constraint form is semantically correct.

**Resolves:** Part of G-09 (implicit in the composite constraint implementation decision).

**Alternatives considered:**
(a) Always use `CREATE UNIQUE INDEX` for simplicity. Rejected: this silently removes the ability
to use `ON CONFLICT ON CONSTRAINT` and FK-targeting, which developers expect from a "unique
constraint."

**Impact:** SQL emitter (Phase 7 T4). `docs/spec/migrations.md` §10.4 down-migration table.

---

### R-18: Destructive classifier operation-to-bucket mapping is locked

**Priority:** P1

**Recommendation:** Djogi MUST implement a two-bucket destructive classifier with the following
operation-to-bucket assignment. This supersedes the informal "emit a warning and require
`--allow-destructive`" language in `docs/spec/migrations.md` §10.2:

| Operation | Bucket | Notes |
|---|---|---|
| `DROP TABLE` | `unexecutableSteps` | Requires `--allow-destructive` |
| `DROP COLUMN` | `unexecutableSteps` | Requires `--allow-destructive` |
| `ALTER COLUMN` nullable → `NOT NULL` without `DEFAULT` | `unexecutableSteps` | Data probe needed |
| Enum value deletion | `unexecutableSteps` | Cannot safely reorder in-place |
| Enum value reorder | `unexecutableSteps` | Not `warnings` — Postgres cannot reorder without recreating |
| `ALTER COLUMN` type narrowing (e.g., `TEXT` → `VARCHAR(50)`) | `warnings` | Emit warning comment in UP file per R-19 |
| `DROP INDEX` | `warnings` | Performance risk, not data loss |
| `DROP FOREIGN KEY` | `warnings` | Referential integrity risk |
| `RENAME COLUMN` without `#[field(renamed_from)]` annotation | `unexecutableSteps` | Without annotation, treated as DROP+ADD |
| `RENAME COLUMN` with `#[field(renamed_from)]` annotation | `warnings` | Annotated rename is explicit |
| `ADD UNIQUE CONSTRAINT` on non-empty table | `warnings` | Postgres enforces; will error at DDL time if duplicates exist |

`unexecutableSteps` MUST hard-block migration file generation unless `--allow-destructive` is
passed. `warnings` MUST surface in `cargo djogi plan` output and appear as `-- DJOGI WARNING:`
comment lines in the UP file immediately before the statement.

**Rationale:** `topics/09-destructive-and-lossy-classification.md` §Prisma classifier is the
primary source. The research recommends promoting `DROP TABLE` and `DROP COLUMN` from Prisma's
`warnings` bucket to Djogi's `unexecutableSteps` because data loss from these operations is
instant and irreversible. Enum value reorder is escalated from `warnings` to `unexecutableSteps`
because Postgres cannot reorder enum values in-place — any attempt requires `CREATE TYPE AS ENUM`,
backfill, and rename, which Djogi should not silently generate. Gap G-11 identifies the full
bucket mapping as missing from SPEC-M.

**Resolves:** G-11 (destructive classifier operation-to-bucket mapping).

**Alternatives considered:**
(a) Follow Prisma's exact bucket assignments (DROP TABLE and DROP COLUMN as `warnings`). Rejected:
Djogi takes the stronger position that irreversible data loss requires explicit acknowledgement,
not just a warning.
(b) A single-bucket "require --allow-destructive for all destructive operations." Rejected: this
blocks harmless operations like `DROP INDEX` behind the same flag as `DROP TABLE`. The two-bucket
model provides appropriate granularity.

**Impact:** Guard module `guard.rs` (Phase 7 T3). `docs/spec/migrations.md` §10.2 rewritten.

---

### R-19: Destructive and lossy warning comments appear in generated UP files

**Priority:** P1

**Recommendation:** Generated UP migration files MUST include `-- DJOGI WARNING:` comment lines
immediately before any statement classified as `warnings` (from R-18). The format is:

```sql
-- DJOGI WARNING: type narrowing — ALTER COLUMN TEXT → VARCHAR(50) may truncate values
--  exceeding 50 characters in existing rows.
ALTER TABLE vehicles ALTER COLUMN description TYPE VARCHAR(50);
```

This is in addition to the existing DOWN file warning (already present in SPEC-M §10.4).
The UP file warning is what code reviewers see — they typically open the UP file, not the
DOWN file, during PR review.

**Rationale:** `topics/09-destructive-and-lossy-classification.md` §Warning comment pattern
notes that Prisma embeds destructive/lossy warnings as SQL comments in generated files.
Gap G-12 identifies that the existing SPEC-M has warnings only in DOWN files, not UP files.
Code review is the primary safety gate for migration review — reviewers must see the warning
when they open the file they are reviewing.

**Resolves:** G-12 (warnings in generated UP SQL as comments).

**Alternatives considered:**
(a) Surface warnings only via `cargo djogi plan` output. Rejected: plan output is ephemeral;
SQL file comments are permanent and persist in version control.
(b) Put warnings in both the UP and DOWN file headers. Rejected: headers are for metadata
(Migration, Direction, Execution-Mode, Snapshot-Base). Inline comments before the affected
statement are more specific and easier to locate.

**Impact:** SQL emitter (Phase 7 T4). `docs/spec/migrations.md` §10.4 updated.

---

### R-20: `#[field(renamed_from)]` is a migration-window-only marker; must be removed after apply

**Priority:** P1

**Recommendation:** The `#[field(renamed_from = "old_name")]` annotation (and its model-level
counterpart `#[model(renamed_from = "old_table")]` from R-16) is a migration-window-only marker.
It MUST be removed from the source code after the rename migration is generated and successfully
applied.

The differ MUST detect a stale annotation (the annotation is present in the current model,
but the snapshot already reflects the rename) and emit an error:

```
error[D008]: stale rename annotation on field `name`
 --> src/apps/vehicles/models.rs:12:9
  |
12 |   #[field(renamed_from = "full_name")]
  |       ^^^^^^^^^^^^^^^^^^^^^^^^^^^ snapshot already reflects rename from "full_name" to "name"
  |
  = help: this annotation was applied in a previous migration; remove it from the source code
```

This approach matches Django's model: `RenameField` operations appear in migration files as
permanent history, but the live model definitions do not carry `renamed_from` metadata after
the migration is applied. Djogi's variant is that the annotation is in the source code
(not the migration file) during the window, and must be removed afterward.

**Rationale:** `topics/07-rename-handling.md` §Annotation lifecycle raises this as an open
question. If the annotation stays permanently, the differ must track rename history to avoid
generating a second rename migration on the next run. If it is removed, the differ only needs
to detect the snapshot state. Detecting the stale annotation (presence in source, already
reflected in snapshot) is simpler and safer than maintaining a rename history. Gap G-08 is
the formal citation.

**Resolves:** G-08 (lifecycle of `#[field(renamed_from)]` after migration).

**Alternatives considered:**
(a) Allow the annotation to remain permanently as documentation. Rejected: this requires the
differ to maintain rename history and detect "already applied" vs "not yet applied" state for
every annotation. The stale-detection approach is a special case of this but with a hard error
rather than silent ignore.
(b) Emit a warning (not an error) for stale annotations. Rejected: a silent warning will be
ignored. Stale annotations must be cleaned up to maintain the source-code-is-current-truth
invariant.

**Impact:** Differ (Phase 7 T1). `docs/spec/models.md` updated with annotation lifecycle.
`docs/spec/decisions.md` updated (the existing field rename decision does not address lifecycle).

---

### R-21: Partial/functional index support in `IndexSpec` — Phase 7 scope, raw escape hatch for complex cases

**Priority:** P1

**Recommendation:** Djogi MUST support partial indexes and simple functional indexes in Phase 7
with the following extensions to `IndexSpec`:

```rust
struct IndexSpec {
  name: Option<String>,    // None = auto-generated per R-15
  columns: Vec<String>,    // column names in declared order
  unique: bool,

  // Partial index: WHERE clause (raw SQL string — no Djogi DSL)
  where_clause: Option<String>, // e.g., Some("status = 'active'")

  // Functional index: expression (raw SQL string)
  // When expression is Some, columns must be empty
  expression: Option<String>,  // e.g., Some("LOWER(email)")

  // Non-transactional execution required (CONCURRENTLY or similar)
  requires_out_of_transaction: bool,

  // Extension dependency (e.g., "pg_trgm" for GIN trigram indexes)
  extension_dependency: Option<String>,
}
```

For partial indexes, the `where_clause` string is emitted verbatim into the generated SQL:
```sql
CREATE INDEX vehicles_active_idx ON vehicles (status) WHERE status = 'active';
```

For functional indexes, the `expression` string is emitted verbatim:
```sql
CREATE INDEX users_lower_email_idx ON users (LOWER(email));
```

Complex expression indexes referencing multiple columns with custom functions require the raw
escape hatch (`ctx.raw_execute(...)`) and are not auto-generated in Phase 7.

**Rationale:** `topics/08-composite-uniques-and-indexes.md` §Partial and functional indexes
documents that no surveyed Rust migration system supports partial or functional indexes at the
descriptor level. This is a genuine Djogi differentiator. Gap G-14 identifies this as a Phase 7
scope decision. The `where_clause` and `expression` as raw strings (not a typed DSL) is the
practical choice — a full SQL expression DSL is too large for Phase 7 scope, but the raw string
approach covers the common cases (simple where clauses, single-column functional indexes).

**Resolves:** G-14 (partial/functional index descriptor representation).

**Alternatives considered:**
(a) Defer entirely to v0.2. Rejected: partial indexes on status columns and functional indexes
on email columns are common enough in production systems that leaving them as raw-escape-hatch-only
would force developers to maintain raw SQL for a routine pattern.
(b) Full typed DSL for WHERE clauses. Rejected: this is a significant scope expansion that would
require a SQL expression IR, which belongs in a later phase.

**Impact:** `IndexSpec` type (Phase 7 T1). SQL emitter (Phase 7 T4). `docs/spec/migrations.md`
updated with `IndexSpec` definition.

---

### R-22: JSONB subfield index support via `json_path` in `IndexSpec`

**Priority:** P1

**Recommendation:** `IndexSpec` MUST support JSONB path indexes via a `json_path` field:

```rust
struct IndexSpec {
  //... (fields from R-21)...
  json_path: Option<String>, // e.g., Some("vin") → generates ON t ((data->>'vin'))
}
```

When `json_path` is set, `columns` MUST contain exactly one column (the JSONB column name),
and the generated SQL uses the JSONB path operator:
```sql
CREATE INDEX vehicles_vin_idx ON vehicles ((metadata->>'vin'));
```

The `json_path` value is a simple dot-separated path (e.g., `"address.city"`), not raw SQL.
For nested paths, Djogi generates the appropriate nested `->` and `->>` operators.

**Rationale:** `topics/11-diff-algorithms.md` §JSONB and custom types notes that `Jsonb<T>`
fields can have path-based indexes and that no surveyed Rust system handles these. Gap G-15
identifies this as a Phase 7 scope item. The `IndexSpec::extension_dependency` and
`IndexSpec::requires_out_of_transaction` fields are already referenced in
the local Phase 7 migration-system v2 implementation plan §In Scope for Phase 7
(under "Phase 6 `IndexSpec::requires_out_of_transaction`" and "Phase 6 `IndexSpec::extension_dependency`").

**Resolves:** G-15 (JSONB subfield index descriptor representation).

**Alternatives considered:**
(a) Require the raw escape hatch for all JSONB path indexes. Rejected: JSONB subfield indexes
are a core use case for `Jsonb<T>` fields, and requiring raw SQL for a pattern that appears
in every Djogi application that uses JSONB defeats the purpose of the descriptor-driven system.
(b) Support full JSONB query expressions as an `expression` string (from R-21). Accepted as
the escape hatch for complex JSONB expressions; `json_path` handles the common simple case.

**Impact:** `IndexSpec` (Phase 7 T1). SQL emitter (Phase 7 T4). `docs/spec/migrations.md`
updated.

---

### R-23: Runner uses a dedicated single connection (not pool) for the migration apply window

**Priority:** P1

**Recommendation:** The migration runner MUST acquire a dedicated single `tokio-postgres`
connection (not from the `deadpool-postgres` pool) for the duration of the migration apply
operation. The connection lifecycle is:
1. Open a new `tokio-postgres` connection using the database URL from config.
2. Acquire the advisory lock on this connection.
3. Execute all migration segments on this connection.
4. Update the ledger and snapshot on this connection.
5. Release the advisory lock explicitly via `pg_advisory_unlock(...)`.
6. Close the connection.

If the process is killed between steps 3 and 5, Postgres automatically releases the advisory
lock when the TCP connection is torn down by the OS. The `pending` ledger row (from R-06) acts
as the crash-detection mechanism.

The pool-connection approach is incorrect because: `deadpool-postgres` recycles connections,
meaning a connection that held an advisory lock could be returned to the pool after the
migration runner exits abnormally, keeping the lock alive until the pool's keep-alive timeout.

**Rationale:** `topics/04-advisory-locks-and-concurrency.md` §deadpool and advisory lock lifecycle
identifies this precise problem. Session-scoped advisory locks are held by the connection, not
by the transaction. If the connection is pooled and recycled, the advisory lock persists until
the pool drops the connection. The dedicated-connection approach guarantees that lock release
is tied to the migration runner's lifecycle, not the pool's lifecycle. Gap G-04 is the formal citation.

**Resolves:** G-04 (deadpool-postgres advisory lock release behavior).

**Alternatives considered:**
(a) Use `pg_try_advisory_xact_lock` (transaction-scoped) so the lock releases automatically
at transaction commit/rollback. Rejected: for non-transactional migrations, there is no enclosing
transaction. A transaction-scoped lock would release mid-migration.
(b) Use the pool but explicitly release the lock in a `drop` handler. Rejected: `drop` handlers
cannot be guaranteed to run in all crash scenarios (OOM kills, SIGKILL). The dedicated connection
with OS-level TCP teardown is the reliable mechanism.

**Impact:** Runner (Phase 7 T5). `Djogi.toml` migration section (dedicated connection URL option
if different from pool URL).

---

### R-24: `cargo djogi verify` is added to the CLI spec

**Priority:** P1

**Recommendation:** `cargo djogi verify` MUST be added to the Phase 7 CLI surface. Behavior:

1. Reads `migrations/schema_snapshot.json`.
2. Connects to the target database (uses the `url` from `Djogi.toml`).
3. Queries `information_schema` and `pg_catalog` to enumerate actual live columns, indexes,
  constraints, enums, and extensions.
4. Compares the live catalog against the snapshot.
5. Reports discrepancies:
  - Objects in snapshot but not in live DB: "snapshot declares X but live DB lacks it."
  - Objects in live DB but not in snapshot: "live DB has X but snapshot does not declare it."
6. Exit code 0 if live DB matches snapshot exactly. Non-zero if any discrepancy.
7. Does NOT modify any state (read-only).

This command bridges the gap left by rejecting the shadow database approach. It is useful after:
- baseline adoption (verify that the existing DB matches the declared schema)
- manual DDL changes by ops
- incident recovery

**Rationale:** `topics/11-diff-algorithms.md` §Shadow DB alternative identifies `djogi verify`
as the Djogi alternative to Prisma's shadow-DB drift detection. Gap G-19 identifies that this
command is absent from the P7V2 CLI surface. Prisma's `diagnose_migration_history` RPC covers
the same use case; Djogi's implementation does not need a shadow DB because it compares against
the snapshot (an explicit artifact) rather than replaying migrations.

**Resolves:** G-19 (`djogi verify` command not in CLI spec).

**Alternatives considered:**
(a) Shadow-DB approach (Prisma). Rejected: requires `CREATE DATABASE` permission, complex
infrastructure setup, and a temporary DB that must be cleaned up. Not acceptable as a default
developer workflow.
(b) Defer to v0.2. Rejected: `verify` is especially needed immediately after baseline adoption,
which is a first-run workflow for every team adopting Djogi on an existing database.

**Impact:** New CLI command (Phase 7 T7/T8). `docs/spec/migrations.md` new §10.9.

---

### R-25: Adopt `HistoryDiagnostic` taxonomy for `plan` and `show` output

**Priority:** P1

**Recommendation:** `cargo djogi plan` and `cargo djogi migrate show` MUST produce structured
diagnostic output using the following three-state taxonomy (adapted from Prisma's
`HistoryDiagnostic` type):

- `DatabaseIsBehind` — the ledger and snapshot have applied migrations, but pending migration
 files exist that have not been applied. This is the normal "there is work to do" state.
 Severity: informational. Default action: `cargo djogi migrate`.
- `UnexpectedHistory` — the filesystem has migration files that are not in the ledger, OR the
 ledger has rows with no corresponding migration file. Severity: warning. Default action:
 investigate before proceeding.
- `HistoryDiverged` — the ledger contains a checksum mismatch for an applied migration. The
 applied SQL does not match the stored checksum. Severity: error. Default action: block
 migration; run `cargo djogi migrate repair`.

Each diagnostic state is reported with the full list of affected migration versions.

**Rationale:** `topics/01-source-of-truth-and-state.md` §Adopt: Three history-diagnostic states
from Prisma quotes the Prisma `HistoryDiagnostic` discriminated union and calls it "the minimal
information-preserving taxonomy." Gap G-20 identifies this as absent from Djogi's plan/show output.

**Resolves:** G-20 (Prisma HistoryDiagnostic taxonomy not adopted).

**Alternatives considered:**
(a) Generic error messages without structured state. Rejected: unstructured errors make
automation and CI integration harder. Structured diagnostics allow scripts to key off the
diagnostic type.
(b) Direct copy of Prisma's TypeScript type names. Rejected: Djogi's names are adjusted to
be more self-explanatory (`UnexpectedHistory` vs Prisma's `migrationsDirectoryHasUnexpectedHistory`).

**Impact:** CLI plan/show (Phase 7 T7). Add diagnostic type to `docs/spec/migrations.md`.

---

### R-26: `schema_snapshot.json` includes a `format_version` field

**Priority:** P1

**Recommendation:** The `schema_snapshot.json` format MUST include a top-level `format_version`
integer field. The initial value is `1`. When the snapshot format changes in a breaking way,
`format_version` is incremented. The runner MUST reject snapshots with an unknown
`format_version` with a clear error:

```json
{
 "format_version": 1,
 "version": "0005",
 "migrated_at": "2026-04-22T10:00:00Z",
 "models": {... }
}
```

After a branch merge that produces a merge conflict in `schema_snapshot.json`, the developer
MUST run `cargo djogi makemigrations` to re-derive the snapshot from the ordered migration set.
This regenerates a conflict-free snapshot. The developer workflow is:
1. Resolve the migration file conflict (if any).
2. Run `cargo djogi makemigrations` to rebuild the snapshot.
3. Commit the updated snapshot.

**Rationale:** `topics/11-diff-algorithms.md` §Snapshot merge conflicts recommends a
`format_version` field from day one. Gap G-18 identifies the snapshot merge conflict resolution
workflow as absent from the current spec. The snapshot's `"version"` field already present in
SPEC-M §10.3 records the migration version, not the format version — these are two distinct
concepts.

**Resolves:** G-18 (snapshot merge conflict resolution strategy and format versioning).

**Alternatives considered:**
(a) No format versioning; rely on the runner to detect incompatible shapes. Rejected: silent
incompatibility is worse than a version-gated error. Explicit versioning enables future format
changes without guessing.
(b) Embed the format version in the migration snapshot filenames. Rejected: the filename is
fixed (`schema_snapshot.json`) and changing it would break existing tooling.

**Impact:** `snapshot.rs` (Phase 7 T1). `docs/spec/migrations.md` §10.3 updated.

---

## Part III: P2 Recommendations (Defer to v0.2+)

---

### R-27: `NULLS NOT DISTINCT` index modifier — defer to v0.2

**Priority:** P2

**Recommendation:** `NULLS NOT DISTINCT` (Postgres 15+, always available on Djogi's Postgres 18
target) is deferred to v0.2+. The composite index attribute syntax from R-14 MUST reserve
space for a future `nulls_not_distinct: bool` option in `#[model(indexes(unique(..., nulls_not_distinct = true)))]`,
but the implementation is not required for v0.1.

**Rationale:** Gap G-21 confirms this as a `Post-0.1.0` item. `NULLS NOT DISTINCT` is useful
for optional-unique patterns (where NULL means "no value assigned, not a duplicate"), but it
is not a blocking need for v0.1.0 adoption. Reserving space in the syntax prevents a future
breaking change to the attribute format.

**Resolves:** G-21 (NULLS NOT DISTINCT support deferred).

**Impact:** Document gap in `docs/spec/migrations.md` as a known v0.2 candidate.

---

### R-28: Online-safe mode with automatic `CONCURRENTLY` injection — defer to Phase 7.5

**Priority:** P2

**Recommendation:** Defer automatic `CREATE INDEX CONCURRENTLY` injection and staged live
migration plans to Phase 7.5. In v0.1, blocking `CREATE INDEX` is the default. Operators
who need `CONCURRENTLY` MUST hand-edit the generated SQL and add `-- djogi:no-transaction`
(R-08).

**Rationale:** `topics/10-online-safe-staged-migrations.md` confirms no surveyed system
fully automates online-safe migrations. Phase 7.5 (per SPEC-AR and P7D §Required Before 0.1.0)
is the planned home for the five staged live-migration patterns. This recommendation preserves
the deferral while ensuring v0.1 operators are not blocked.

**Impact:** Document v0.1 limitation in `docs/spec/migrations.md`. Phase 7.5 spec is a
separate document.

---

### R-29: Shadow-DB drift detection via `--live` flag on `cargo djogi migrate check` — defer to v0.2

**Priority:** P2

**Recommendation:** A full shadow-DB or live-catalog drift detection command (beyond `cargo djogi verify`)
is deferred to v0.2. `cargo djogi verify` (R-24) provides the v0.1 snapshot-vs-live comparison.
A command that detects drift introduced by out-of-band DDL on the live DB (without a shadow DB)
requires a catalog introspection layer not yet designed.

**Rationale:** `topics/01-source-of-truth-and-state.md` §Defer: Shadow-DB drift detection
explicitly defers this to `0.2.0`. The `djogi verify` command (R-24) provides the immediate
need.

**Impact:** Document as v0.2 candidate.

---

### R-30: Snapshot merge conflict resolution tooling (`cargo djogi reconcile`) — defer to v0.2

**Priority:** P2

**Recommendation:** A dedicated `cargo djogi reconcile` command that automates post-merge
snapshot conflict resolution is deferred to v0.2. The v0.1 workflow is manual (run
`cargo djogi makemigrations` after merge per R-26). The reconcile command would automate
the detection and resolution of diverged snapshot states in multi-branch environments.

**Rationale:** Gap G-18 Priority-3 in document 13 places snapshot merge conflict tooling as
pre-0.1.0 documentation but not necessarily pre-0.1.0 implementation. The `format_version`
field (R-26) and `makemigrations` post-merge workflow are sufficient for v0.1.

**Impact:** Document workflow in `docs/spec/migrations.md`. Defer command to v0.2.

---

### R-31: Missing-migration-file detection and `MISSING` ledger state — P1 in implementation, diagnostic only

**Priority:** P2 (implementation priority; diagnostic behavior is P1)

**Recommendation:** The full `MISSING` ledger state (a distinct `status` value for migrations
present in the ledger but absent from disk) is deferred to v0.2 as a `status` enum extension.
In v0.1, the runner MUST detect a missing file (ledger row present, file absent) and emit
a hard error before any further migration is attempted, but it does NOT need to write a
`MISSING` status row:

```
error[M015]: migration file missing for applied migration "0005_add_payment_table"
 = help: the ledger records this migration as applied, but no file exists at:
 =  migrations/0005_add_payment_table_up.sql
 = help: if this file was deliberately removed, run `cargo djogi migrate repair`
 =    with operator confirmation
```

**Rationale:** Gap G-16 is Priority-2 in document 13. The Flyway `MISSING_SUCCESS` state is
the prior art. In v0.1, a hard error is sufficient — the `MISSING` status value adds queryability
that is valuable but not blocking. The `status` CHECK constraint from R-04 does not include
`MISSING` and must not be extended without a spec update.

**Resolves:** G-16 (missing migration file policy — partially, via error-before-apply behavior).

**Impact:** Runner (Phase 7 T5). `status` enum extension deferred to v0.2 spec update.

---

## Part IV: Explicit Rejections

---

### X-01: Do not embed snapshots inside migration files (cot pattern)

**Rejection:** Djogi MUST NOT embed schema snapshot structs inside migration files as
`#[model(model_type = "migration")]` annotated structs. The `cot-cli/src/migration_generator.rs`
approach (where each migration file contains both a snapshot of the model at generation time
and a `const OPERATIONS` list) couples the execution plan to the snapshot. Hand-editing either
the operations list or the snapshot struct corrupts future diffs.

**Rationale:** `topics/11-diff-algorithms.md` §Approach A (ORM-model-canonical) and
`topics/12-rust-ecosystem-contrast.md` §cot limitations document the coupling problem: cot
hits `todo!()` at `migration_generator.rs:835` on field type changes because the snapshot struct
shape cannot represent all `ColumnType` variants. The snapshot-in-migration design also makes
the migration file non-SQL, conflicting with Djogi's SQL-first review artifact decision (P7D).
Djogi's `migrations/schema_snapshot.json` side-car file avoids all of these problems.

**Topic citations:** `topics/11-diff-algorithms.md`, `topics/12-rust-ecosystem-contrast.md`.

---

### X-02: Do not use Alembic's current-state-pointer ledger design

**Rejection:** Djogi MUST NOT use a single-row "current state" ledger (like Alembic's
`alembic_version` which stores only `version_num` — the current head, not a log). Every
Djogi migration MUST leave a permanent row in the ledger as an audit record.

**Rationale:** `topics/02-ledger-schema.md` §History log vs current-state pointer establishes
that Alembic's table is a current-state pointer, not a history log. It stores no timestamp,
no checksum, and no execution record. This makes post-incident investigation impossible:
"which migrations ran in last Friday's deploy and how long did they take?" is unanswerable.
Flyway, Prisma, Liquibase, and Django all maintain full history logs.

**Topic citations:** `topics/01-source-of-truth-and-state.md`, `topics/02-ledger-schema.md`.

---

### X-03: Do not use Flyway's `CRC-32` checksum or refinery's `SipHash-1-3` checksum

**Rejection:** Djogi MUST NOT use CRC-32 (Flyway) or SipHash-1-3 (refinery) for migration
file checksums. SHA-256 (R-05) is the required algorithm.

**Rationale:** CRC-32's 32-bit collision domain is non-negligible at scale; signed-integer
storage (`INTEGER` in Postgres) requires sign-handling on reads. SipHash-1-3 is not a
cryptographic hash and produces non-cryptographic output; refinery's use of name+version+sql
as combined hash input is the key anti-pattern (renaming a file changes the checksum even
when SQL is unchanged). Both algorithms are technically weaker than SHA-256 and both have
practical design flaws for the migration use case.

**Topic citations:** `topics/03-checksums-and-repair.md`.

---

### X-04: Do not use Liquibase's dedicated lock table for concurrency control

**Rejection:** Djogi MUST NOT use a separate lock table (like Liquibase's `DATABASECHANGELOGLOCK`)
for migration concurrency control. Postgres advisory locks (R-03) are the required mechanism.

**Rationale:** `topics/04-advisory-locks-and-concurrency.md` §Approach B documents Liquibase's
lock table: it has no automatic crash recovery, requires a manual `releaseLocks` call when
a process crashes while holding the lock, and creates a permanent operational maintenance burden.
Postgres advisory locks release automatically on disconnect, making them strictly superior for
a Postgres-only system. Djogi is Postgres-only permanently (SPEC-D), so the advisory lock
approach has no downside.

**Topic citations:** `topics/04-advisory-locks-and-concurrency.md`.

---

### X-05: Do not hash migration filename or version number into the content checksum

**Rejection:** Djogi's checksum computation MUST NOT include the migration filename, version
string, or description as inputs to the SHA-256 hash. Only the SQL content (after normalization)
is hashed.

**Rationale:** Refinery's `SipHasher13::new(); name.hash(&mut hasher); version.hash(&mut hasher); sql.hash(&mut hasher)`
(`refinery_core/src/runner.rs:92-96`) means renaming a migration file changes the checksum even
when the SQL is byte-for-byte identical. This breaks checksum continuity on routine file
renaming operations (e.g., correcting a typo in the migration description). The content-only
hash approach (Prisma's) correctly identifies when the SQL itself has changed, not when the
file's metadata has changed.

**Topic citations:** `topics/03-checksums-and-repair.md`.

---

## Part V: Spec Update Manifest

| Recommendation | Spec doc | Action | Sections affected |
|---|---|---|---|
| R-01 | `docs/spec/migrations.md` | Rewrite §10.1 bullet 2: replace "sqlx's built-in runner" | §10.1 |
| R-01 | `docs/spec/decisions.md` | Add decision row: "Migration runner — Djogi-owned over tokio-postgres" | New row |
| R-02 | `docs/spec/migrations.md` | Remove every occurrence of `_sqlx_migrations` | §10.1, any occurrence |
| R-02 | `docs/spec/decisions.md` | Add decision row: "Ledger table name — `djogi_schema_migrations`" | New row |
| R-03 | `docs/spec/migrations.md` | Add new §10.7 (lock key) | §10.7 (new) |
| R-03 | `docs/spec/decisions.md` | Add decision row: "Advisory lock key — `0x444A4F474D494752`" | New row |
| R-04 | `docs/spec/migrations.md` | Add new §10.7 (ledger DDL) with finalized DDL | §10.7 (new) |
| R-04 | Phase 7 v2 plan | Replace §Ledger shape DDL entirely | §Ledger shape |
| R-05 | `docs/spec/migrations.md` | Add §10.1 checksum subsection | §10.1 |
| R-05 | `docs/spec/decisions.md` | Add decision row: "Checksum algorithm — SHA-256, V1: prefix, content-only" | New row |
| R-06 | `docs/spec/migrations.md` | Add §10.7 pre-write row pattern | §10.7 |
| R-07 | `docs/spec/migrations.md` | Update §10.3 with marker file protocol | §10.3 |
| R-08 | `docs/spec/migrations.md` | Update §10.4 file headers with directive specification | §10.4 |
| R-09 | `docs/spec/migrations.md` | Update §10.2 with out-of-order policy tiers | §10.2 |
| R-09 | `docs/spec/decisions.md` | Update "Schema snapshot" row with env-sensitive policy | Existing row |
| R-10 | `docs/spec/migrations.md` | Add §10.8 baseline and fake flows | §10.8 (new) |
| R-11 | `docs/spec/migrations.md` | Rewrite §10.6 SchemaDelta enum entirely | §10.6 |
| R-12 | `docs/spec/migrations.md` | Update §10.2 build.rs behavior to diagnostic-only | §10.2 |
| R-12 | `docs/spec/decisions.md` | Update "Build drift diagnostic" and "Migration generation" rows | Two existing rows |
| R-13 | `docs/spec/migrations.md` | Add rollback ordering rule to §10.2 | §10.2 |
| R-13 | `docs/spec/decisions.md` | Add decision row: "Rollback ordering — by `id` column (temporal)" | New row |
| R-14 | `docs/spec/models.md` | Add `#[model(indexes(...))]` syntax section | New section |
| R-15 | `docs/spec/migrations.md` | Add naming algorithm to §10.2 or new §10.9 | §10.9 (new) |
| R-16 | `docs/spec/models.md` | Add `#[model(renamed_from = "...")]` to annotation list | Existing annotations section |
| R-16 | `docs/spec/decisions.md` | Add decision row: "`#[model(renamed_from)]` — model-level rename annotation" | New row |
| R-17 | `docs/spec/migrations.md` | Add unique constraint SQL form note to §10.4 | §10.4 |
| R-18 | `docs/spec/migrations.md` | Rewrite §10.2 destructive operations with full bucket table | §10.2 |
| R-19 | `docs/spec/migrations.md` | Update §10.4 to show UP file warning comment format | §10.4 |
| R-20 | `docs/spec/models.md` | Add rename annotation lifecycle section | New subsection |
| R-21 | `docs/spec/migrations.md` | Add `IndexSpec` definition with `where_clause` and `expression` | §10.9 or new |
| R-22 | `docs/spec/migrations.md` | Add `json_path` to `IndexSpec` | Same as R-21 |
| R-23 | Phase 7 v2 plan | Add dedicated-connection note to runner design | §Runner |
| R-24 | Phase 7 v2 plan | Add `cargo djogi verify` to §CLI Surface | §CLI Surface |
| R-25 | `docs/spec/migrations.md` | Add §10.10 diagnostic taxonomy | §10.10 (new) |
| R-26 | `docs/spec/migrations.md` | Update §10.3 snapshot format with `format_version` | §10.3 |
| R-31 | `docs/spec/migrations.md` | Add missing-file error behavior note to §10.2 | §10.2 |

---

## Part VI: Open Items for User Review

The following items could not be resolved from research alone. Each requires a user decision
before the affected Phase 7 task begins.

---

### OI-01: `run_id` column type and generation method

**Question:** What is the type and generation strategy for `run_id` in the ledger?

**Options:**
- (A) UUID `VARCHAR(36)` — generated by Djogi at the start of each `cargo djogi migrate`
 invocation. Human-readable, globally unique, sortable as string.
- (B) ULID `TEXT` (26 chars) — lexicographically sortable by time, URL-safe, no hyphens.
 Requires a ULID crate dependency.
- (C) Timestamp-prefixed random string (`YYYYMMDDHHMMSS_random8`) — short, human-readable,
 no new dependency.

**Research position:** `topics/02-ledger-schema.md` §`DEPLOYMENT_ID` notes Liquibase uses
a 10-character base-36 encoded timestamp. No specific recommendation for Djogi's format beyond
"not the Liquibase approach because 10 chars is too short for global uniqueness."

**Decision needed before:** Phase 7 T2 (ledger implementation).

---

### OI-02: `down_checksum` nullability semantics for empty down files

**Question:** If `_down.sql` exists but is empty (a stub file with only a comment header),
should `down_checksum` store the checksum of the empty-after-normalization string, or be NULL?

**Context:** The recommendation (R-04) says `down_checksum` is NULL only when no `_down.sql`
file exists. But Djogi always generates `_down.sql` files (even for operations where rollback
is impossible, a stub with a WARNING comment is generated). This means `down_checksum` will
almost always be non-NULL — the stub file gets a checksum of the header comment bytes.

**Decision needed before:** Phase 7 T4 (SQL emitter).

---

### OI-03: `applied_at` value during `baseline` and `--fake` operations

**Question:** When `cargo djogi migrate baseline <version>` is run, what value should `applied_at`
get for the baseline rows?

**Options:**
- (A) `now()` — the time the baseline command ran. Accurate for audit ("Djogi was adopted on
 this date") but confusingly later than when those migrations actually ran.
- (B) `epoch` or a sentinel timestamp — makes it clear these rows were not generated by real
 execution.
- (C) No `applied_at` for baseline rows — set to NULL and add `NULL` to the CHECK constraint.

**Research position:** Flyway's baseline row has `installed_on = now()` (time of baseline
command). Prisma's `markMigrationApplied` sets `started_at = finished_at` (zero elapsed time).
Neither is clearly correct for the "adopted from existing DB" scenario.

**Decision needed before:** Phase 7 T5 (baseline flow implementation).

---

### OI-04: Whether `cargo djogi makemigrations` should also update the snapshot

**Question:** After `cargo djogi makemigrations` generates migration SQL files, should it also
update `migrations/schema_snapshot.json` to reflect the pending-but-not-yet-applied schema?

**Context:** The current spec (SPEC-D) says the snapshot is updated only on successful
`cargo djogi migrate`. If `makemigrations` does not update the snapshot, running `cargo build`
after `makemigrations` (but before `migrate`) will emit the drift warning again (because
`build.rs` still sees a difference between `djogi_models.json` and the snapshot). This could
be confusing.

**Options:**
- (A) `makemigrations` does NOT update the snapshot. Build continues to warn until `migrate`
 succeeds. Consistent with the "snapshot = applied state" invariant.
- (B) `makemigrations` writes a `pending_snapshot.json` alongside the migration files.
 `build.rs` checks for `pending_snapshot.json` and suppresses the warning if it exists and
 matches `djogi_models.json`.

**Decision needed before:** Phase 7 T8 (build.rs integration).

---

### OI-05: `partial_apply_info` vs structured step counter (Prisma's `applied_steps_count`)

**Question:** Should the partial-apply tracking be a free-text `partial_apply_detail TEXT` column
(R-04's recommendation) or a structured step counter (`partial_apply_step_index INT NOT NULL DEFAULT 0,
partial_apply_total_steps INT`)?

**Context:** `topics/02-ledger-schema.md` §Open questions raises this as an open item.
Prisma's `applied_steps_count INTEGER DEFAULT 0` is more machine-readable and enables queries
like "which non-transactional migrations failed at step 3 or later?" The text column is more
flexible but requires parsing.

**Decision needed before:** Phase 7 T2 (ledger DDL is frozen before T5 runner implements
partial-apply tracking).

---

### OI-06: Snapshot update timing for non-transactional migrations

**Question:** For a migration with `execution_mode = 'non_transactional'`, at what precise
point in the execution sequence is `schema_snapshot.json` updated?

**Context:** For transactional migrations, the snapshot update can be part of the same commit.
For non-transactional migrations, each DDL statement auto-commits. The snapshot should not be
updated until the entire migration (all non-transactional segments) completes successfully.
But if the migration spans multiple segments (some transactional, some non-transactional),
what is the snapshot update point?

**Research position:** `topics/01-source-of-truth-and-state.md` §Open question 3 explicitly
identifies this as "needs a precise commit sequence documented in the runner spec."

**Decision needed before:** Phase 7 T5 (runner implementation).

---

*End of locked recommendations.*
