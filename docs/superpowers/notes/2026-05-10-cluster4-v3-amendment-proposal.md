# Proposed Cluster 4 v3 Amendments

> Derived from the Cluster 4 PG18+PostGIS coverage cross-reference
> (companion file `2026-05-10-cluster4-vs-postgres-coverage-xref.md` in
> this directory). Apply via inline edits to the v3 Phase 8.5
> alpha-readiness plan after independent review.

## Path-reference disclaimer

This document cites local reviewer artifacts that are not tracked in
this repository (per `.gitignore` line 27 covering `docs/superpowers/`,
and the unstaged `docs/research/postgres-coverage/2026-05-09/` research
set). Specifically:

- The v3 Phase 8.5 alpha-readiness plan lives at
  `docs/superpowers/plans/2026-05-09-phase8-5-alpha-readiness-v3.md`
  in the user's local working tree and is not committed.
- The PG18+PostGIS coverage research lives at
  `docs/research/postgres-coverage/2026-05-09/01..06.md` plus
  `MASTER-CATALOG.md` / `RESEARCH-SUMMARY.txt` in the user's local
  working tree and is not committed.
- The red-team gate plan lives at
  `docs/superpowers/red-team-gate-plan.md` in the user's local
  working tree and is not committed.

Path references below are accurate for the lane lead's local
filesystem; readers cloning the repository will only see the notes
under `docs/superpowers/notes/`. Each cited research artifact is
also referred to by its descriptive name (e.g., "the v3 Phase 8.5
alpha-readiness plan", "the PG18+PostGIS catalog research") so the
prose remains meaningful without local path resolution.

## Pre-existing GH issues confirmed in scope (re-eval `2026-05-10`)

The following already-filed GH issues cover work that was in earlier
draft amendments. They do NOT need re-filing; the audit should route
them through Cluster 4.0:

- **`djogi#150`** — `framework gap: PG18 temporal constraints
  (WITHOUT OVERLAPS, PERIOD FK, NOT ENFORCED)`. Body explicitly covers
  all four PG18 temporal-constraint primitives (WITHOUT OVERLAPS,
  PERIOD FK, NOT ENFORCED, named NOT NULL) plus the design
  coordination with #148 ("Sibling of #148 — same no-overlap
  motivation, distinct DDL surface area"). #150 is marked "Required
  for Phase 8.5 alpha-readiness." **Reframes the temporal-constraint
  evaluation bullet from Amendment 2 (now removed) and the standalone
  `NOT ENFORCED` line that was in the v2 gap table.**

Other in-scope filed issues that v3 4A–4E already names by number
(unchanged): #71, #72, #84, #85, #88, #89, #92, #94, #95, #99,
#101–#106, #108, #147, #148.

## Reviewer-driven corrections (re-eval `2026-05-10`)

Following careful-coder Opus review (verdict
`ALLOW_WITH_CORRECTIONS`), the following amendments were
reframed or dropped:

1. **Amendment 4 issue 1 (SAVEPOINT)** — was framed as "no SAVEPOINT
   typed surface". Reviewer correctly identified that nested
   `atomic(&mut *outer, |inner| ...)` IS the typed savepoint surface
   (`djogi/src/transaction.rs:13-18, 207-309`). **Dropped from the
   issues-to-file list below** because SAVEPOINT capability already
   exists; recorded as an anchored deferral. The convenience-only
   caller-named `savepoint(name)` method can be filed lazily if/when
   an adopter shape demonstrates the need.
2. **Amendment 2 (temporal-constraint evaluation bullet)** — duplicates
   already-filed `djogi#150`. **Removed below;** Amendment 2 retains
   only the OLD/NEW RETURNING evaluation bullet plus the quick-wins
   tabulation bullet, and the routing-to-#150 instruction.
3. **Amendment 4 issue 3 (pgcrypto)** — was framed as "extension
   absence". Reviewer noted the extension IS reachable via the
   migration emitter allowlist (`djogi/src/migrate/bootstrap.rs:139`).
   **Reframed below** as expression-side typed wrapper gap, NOT
   extension absence. The pgcrypto issue stays in the file-list with
   the corrected scope.

**Net result:** 5 amendments (unchanged), 7 issues to file (was 8),
1 amendment evaluation bullet absorbed by an existing issue (#150).

## Amendment 1: Add MERGE to Cluster 4B

**Section:** "Cluster 4B: Set And Relational SQL Shapes" of the v3
Phase 8.5 alpha-readiness plan (local reviewer artifact at
`docs/superpowers/plans/2026-05-09-phase8-5-alpha-readiness-v3.md`
lines 475–506 at lock time).

**Current text (verified against the v3 plan as of 2026-05-12 re-eval):**
> **Primary issues:** `djogi#101`, `djogi#102`, `djogi#103`, `djogi#104`,
> `djogi#105`, `djogi#106`, `djogi#168`, `djogi#169`, `djogi#170`,
> `djogi#172`.
>
> - [ ] Add typed `UNION`, `INTERSECT`, and `EXCEPT` between compatible
>       `QuerySet`s, with type/column compatibility diagnostics.
> - [ ] Add typed LATERAL join support for user-facing query composition.
> - [ ] Add typed `VALUES` or inline-relation sources usable as join inputs.
> - [ ] Add typed `FOR SHARE` row-lock mode and verify lock SQL shape.
> - [ ] Add `CHECK` constraint declaration through `#[field(...)]` or the
>       descriptor surface, with migration projection tests.
> - [ ] Add `INSERT ... SELECT` support for copying rows from typed queries into
>       typed tables.
> - [ ] `djogi#168`: typed isolation-level surface (`atomic_with(level, ...)`
>       or chosen builder shape), including `40001` retry parity through
>       `retry_on_conflict`. Discovered via #110 round-2 dogfood (cat3_b).
> - [ ] `djogi#169`: typed `SET CONSTRAINTS DEFERRED` surface, validated
>       against named constraints declared on model descriptors, with the
>       transaction-only invariant enforced. Discovered via #110 round-2
>       dogfood (cat3_c).
> - [ ] `djogi#170` (umbrella): file sub-issues for INTERVAL, INET / CIDR /
>       MACADDR, MONEY (or doc decision), RANGE TYPES, and DOMAIN TYPES.
> - [ ] `djogi#172` (umbrella): file sub-issues for COMMENT ON, storage
>       parameters, TABLESPACE, ALTER COLUMN ... TYPE ... USING, and
>       generated-column re-declarations.

**Proposed text (preserves all existing 4B routed items + adds MERGE):**
> **Primary issues:** `djogi#101`, `djogi#102`, `djogi#103`, `djogi#104`,
> `djogi#105`, `djogi#106`, `djogi#168`, `djogi#169`, `djogi#170`,
> `djogi#172`, `djogi#new-merge` (filed during Cluster 4.0 audit; see
> Amendment 1a).
>
> - [ ] Add typed `UNION`, `INTERSECT`, and `EXCEPT` between compatible
>       `QuerySet`s, with type/column compatibility diagnostics.
> - [ ] Add typed LATERAL join support for user-facing query composition.
> - [ ] Add typed `VALUES` or inline-relation sources usable as join inputs.
> - [ ] Add typed `FOR SHARE` row-lock mode and verify lock SQL shape.
> - [ ] Add `CHECK` constraint declaration through `#[field(...)]` or the
>       descriptor surface, with migration projection tests.
> - [ ] Add `INSERT ... SELECT` support for copying rows from typed queries into
>       typed tables.
> - [ ] `djogi#168`: typed isolation-level surface (unchanged).
> - [ ] `djogi#169`: typed `SET CONSTRAINTS DEFERRED` surface (unchanged).
> - [ ] `djogi#170` (umbrella, unchanged).
> - [ ] `djogi#172` (umbrella, unchanged).
> - [ ] Add typed `MERGE INTO ... USING ... WHEN MATCHED ... WHEN NOT
>       MATCHED ...` surface for conditional UPSERT-with-conditions.
>       The Postgres 18+ catalog cross-reference (Cluster 4.0)
>       confirmed no typed surface today; raw SQL is the only path.
>       Acceptance: covers single-source MERGE with at least
>       `WHEN MATCHED THEN UPDATE` and `WHEN NOT MATCHED THEN INSERT`
>       branches, with typed condition predicates.

**Rationale:** The MASTER-CATALOG marks MERGE as `partial` (PG15+
conditional DML), but spec-grep against `djogi/src/` finds zero matches
for `MERGE INTO` / `MergeStmt` / `merge_stmt` / `MergeWhen`. The only
hit is a comment in `migrate/diff.rs:572` that uses the verb in a
different sense. Catalog over-claims; actual coverage is `unknown` /
uncovered. MERGE is a high-adoption-pressure shape that has been
stable since PG15 — leaving it as raw-only contradicts the v3 plan's
"no raw SQL because the typed API is missing" exit clause for Cluster 4.

**Routing source:** `docs/research/postgres-coverage/2026-05-09/03-pg18-sql-reference.md`
line 16 (catalog row); spec-grep evidence in
`2026-05-10-cluster4-vs-postgres-coverage-xref.md` "Cluster 4B" section.

## Amendment 1a: New issue to file before adding to 4B

**Title:** `framework gap: MERGE INTO ... USING ... typed surface`

**Body sketch (matching round-1 #101–#106 format):**

```markdown
## Problem

There is no typed API for `MERGE INTO target USING source ... WHEN
MATCHED ... WHEN NOT MATCHED ...`. PostgreSQL 15+ ships MERGE as the
standard SQL conditional-DML shape; djogi has neither a `Model::merge`
helper nor a `QuerySet::merge_into` combinator.

A concrete shape that is not expressible today:

```rust
// Conditional upsert with branching: insert new rows, update changed rows,
// soft-delete missing rows — all in one round trip
// No .merge_into(...) method exists; adopters fall back to ctx.raw_query(...)
```

## Surfaced at

Cluster 4.0 Postgres 18+ feature gap audit (the PG18+PostGIS coverage
cross-reference). The MASTER-CATALOG marks MERGE as `partial`;
spec-grep confirms no typed surface. Adopters who need MERGE today
fall back to raw SQL.

## Investigation

Searched `djogi/src/` for `MERGE INTO`, `MergeStmt`, `merge_stmt`,
`MergeWhen`. Only one match: a comment in `migrate/diff.rs:572` using
"MERGES" in a different sense (cluster of cross-flipping options). No
QuerySet method, no Model helper, no SQL emitter for MERGE.

`Model::save` covers single-row INSERT-or-UPDATE via primary key;
`ON CONFLICT (...) DO UPDATE` covers single-table UPSERT for primary-key
conflicts. Neither covers MERGE's source-driven conditional semantics
where the source is another table or query result.

## Proposed direction

Add `QuerySet::<T>::merge_into(target_model, on_condition)` returning
a builder that accepts `.when_matched_then_update(...)`,
`.when_not_matched_then_insert(...)`, and `.when_matched_and(cond,
action)` clauses. The result is a terminal that returns the count
of rows by branch (or `RETURNING` row collection if PG18 OLD/NEW
RETURNING is in scope).

## Related

Sibling to #101 (UNION/INTERSECT/EXCEPT — both are missing
multi-statement-shape combinators) and to #106 (INSERT ... SELECT — both
are source-driven write shapes).

## Closing-condition checklist (Stage 1.5)

- [ ] Public API documented in rustdoc on `QuerySet::merge_into` and
      the builder type, with one full worked example
- [ ] Doctest exercises the WHEN MATCHED + WHEN NOT MATCHED minimum
      branch set
- [ ] Spec amendment in `docs/spec/queries.md` (or successor) describes
      the SQL shape, condition typing, and `RETURNING` interaction
- [ ] Live PG18 integration test exercises both branches against a real
      database
- [ ] At least one user guide example in `docs/guide/` (placement TBD)
```

**Cluster:** 4B

## Amendment 2: Add OLD/NEW RETURNING evaluation to Cluster 4.0

**Section:** "Cluster 4.0: Postgres 18+ Gap Preflight"
(`docs/superpowers/plans/2026-05-09-phase8-5-alpha-readiness-v3.md`
lines 361–371)

**Current text:**
> - [ ] This subcluster satisfies Area 19 of
>       `docs/superpowers/red-team-gate-plan.md` and moves that Postgres 18.0+
>       feature audit from release-gate Pass 0 to the pre-Cluster-4 slot.
> - [ ] Run the Postgres 18+ feature gap audit before implementing Cluster 4
>       subclusters.
> - [ ] Route every alpha-blocking gap into the relevant 4A-4E subcluster before
>       implementation starts.
> - [ ] Document true non-goals in the public contract only when they are outside
>       Djogi's v0.1.0 typed-surface promise.

**Proposed text (additions only):**

After the existing four bullets, add:

> - [ ] Evaluate Postgres 18 `OLD` / `NEW` in `RETURNING` for INSERT,
>       UPDATE, DELETE, and MERGE
>       (`docs/research/postgres-coverage/2026-05-09/01-pg18-release-notes.md`
>       row 59). Spec-grep finds no typed surface today. Audit
>       disposition: `Implemented` (none today), `Roadmapped` (route to
>       4B as #4B.8 if alpha-blocking for audit/event-publication
>       shapes), or `Out of scope` with anchored deferral.
> - [ ] Route already-filed `djogi#150`
>       (`framework gap: PG18 temporal constraints (WITHOUT OVERLAPS,
>       PERIOD FK, NOT ENFORCED)`) through this audit. #150's body
>       covers the full PG18 temporal-constraint family (WITHOUT
>       OVERLAPS, PERIOD FK, NOT ENFORCED, named NOT NULL) and is
>       marked "Required for Phase 8.5 alpha-readiness." Coordinate
>       with 4E #148 (the EXCLUDE-vs-WITHOUT-OVERLAPS preference is
>       still surfaced separately by Amendment 3 below).
> - [ ] Tabulate the eight quick-wins from
>       `2026-05-10-cluster4-vs-postgres-coverage-xref.md` and flip
>       their catalog disposition from `unknown` to `Implemented`
>       on first audit pass — they have already been spec-grep
>       verified.

**Rationale:** The Cluster 4.0 contract today is "run the audit; route
alpha-blocking gaps." The cross-reference surfaced one new PG18-specific
item (OLD/NEW RETURNING) that the existing 4A-4E task lists do not name
explicitly. The temporal-constraint family is already filed as #150 —
the audit should route it rather than re-discover it. The quick-wins
tabulation prevents repeat spec-grep work.

**Re-eval correction (`2026-05-10`):** The original Amendment 2 had a
second evaluation bullet for the PG18 temporal-constraint family. That
duplicates already-filed `djogi#150`. The bullet has been replaced with
a routing instruction for the existing issue. Anchor: anchored
deferral via existing GH issue, per the "anchor every deferral" memory
rule.

**Routing source:** `2026-05-10-cluster4-vs-postgres-coverage-xref.md`
"Cluster 4.0" and "High-leverage gaps NOT currently in any v3 cluster"
sections.

## Amendment 3: Note PG18 syntax preference in Cluster 4E

**Section:** "Cluster 4E: Postgres Text Search And Constraint Coverage"
(`docs/superpowers/plans/2026-05-09-phase8-5-alpha-readiness-v3.md`
lines 436–462)

**Current text:** task #148 covers `btree_gist` exclusion-constraint
support and typed range fields (tsrange / tstzrange / daterange / int
ranges).

**Proposed text (insert as a new sub-task between the current #148
range-field bullet and "Prove both features..."):**

> - [ ] When emitting temporal-constraint DDL for the no-overlap shape,
>       prefer Postgres 18's native `WITHOUT OVERLAPS` form over an
>       `EXCLUDE USING gist (... WITH &&)` shape when both express the
>       same intent. Postgres 18 is djogi's supported floor
>       (`docs/spec/decisions.md`), so the more declarative form is the
>       canonical emission.
> - [ ] If the typed `#[field()]` attribute surface for exclusion
>       constraints is more ergonomic than a `WITHOUT OVERLAPS`-aware
>       constraint declaration, document the divergence: typed range
>       exclusion remains the broader user-facing API; PG18 temporal
>       syntax is one specific lowering target the migration emitter
>       chooses.

**Rationale:** Cluster 4E #148 task wording targets the EXCLUDE form
exclusively, but PG18 `PRIMARY KEY (id, validity WITHOUT OVERLAPS)`
expresses the same intent more declaratively. With PG18 as the floor,
the emitter should reach for the modern form when it fits the user's
descriptor. Failing to acknowledge this leaves an interop hole where
djogi-emitted DDL is structurally more verbose than necessary.

**Routing source:** `docs/research/postgres-coverage/2026-05-09/01-pg18-release-notes.md`
lines 49–50 and `MASTER-CATALOG.md` "Notable Cluster 1: PG18 Constraint
& Temporal Features" (line 129–134).

## Amendment 4: File post-v0.1.0 tracking issues for known carve-outs

**Section:** "Cluster 4.0: Postgres 18+ Gap Preflight" — add a final
task bullet.

**Proposed addition:**

> - [ ] Before closing Cluster 4.0, file the following four tracking
>       issues so the post-v0.1.0 backlog has them on the books and the
>       no-arbitrary-deferrals rule is satisfied:
>       1. `framework gap: FTS configuration DDL (CREATE TEXT SEARCH
>          CONFIGURATION / DICTIONARY / PARSER / TEMPLATE)` — anchor:
>          declarative low-frequency surface; `tsvector` / `tsquery`
>          / `to_tsvector` / `to_tsquery` typed query surface IS
>          implemented; raw-SQL bypass attribute acceptable for the
>          DDL during v0.1.0.
>       2. `framework gap: pgcrypto expression-side typed wrapper
>          (encrypt / decrypt / digest / hmac / gen_salt etc.) — the
>          extension itself IS reachable via the migration emitter
>          allowlist (djogi/src/migrate/bootstrap.rs:139); only the
>          typed expression surface is missing` — anchor: adopters
>          use Rust-side crypto today (sha2, hmac, ring); pgcrypto
>          server-side wrapping is post-v0.1.0 only if a real adopter
>          shape requires it. NOT a CREATE EXTENSION absence — the
>          differ already projects pgcrypto as an extension dependency.
>       3. `framework gap: PG18 scalar functions (uuidv7, uuidv4,
>          array_sort, array_reverse, casefold, crc32, crc32c, gamma,
>          lgamma)` — anchor: HeerId/RanjId obviates UUID surface;
>          remaining scalars are roadmapped only when an adopter
>          shape needs them.
>       4. `framework gap: MIN()/MAX() over arrays and composites
>          (PG18)` — anchor: route as 4C tail-task after #89
>          type-state migration lands the Kind discriminator; until
>          then, raw SQL bypass is acceptable.
>
> **Note:** A fifth candidate — caller-named `savepoint(name)`
> ergonomics on top of nested `atomic()` — was deliberately NOT
> filed in this audit. SAVEPOINT capability already exists at
> `djogi/src/transaction.rs:13-18, 207-309` (nested `atomic()`
> pushes `SAVEPOINT sp_<depth>` / `RELEASE` / `ROLLBACK TO`).
> A caller-supplied opaque name is convenience-only and should be
> filed lazily if/when an adopter shape demonstrates the need. This
> is anchored deferral via "capability already exists; ergonomics
> shortcut tracked informally as a v0.1.0+ enhancement candidate."

**Rationale:** The cross-reference identified four categorical gaps
that none of 4A-4E covers and that no existing GH issue tracks.
Filing tracking issues during the audit satisfies the "no arbitrary
deferrals" rule (each has an explicit anchor) and gives the
post-v0.1.0 backlog a known shape rather than discovery-by-customer-bug.

**Re-eval corrections (`2026-05-10`, post-Opus review):**
- The original Amendment 4 listed five issues. Issue 1 (SAVEPOINT)
  has been removed from the file-list because reviewer correctly
  identified that nested `atomic()` IS the typed savepoint surface
  (`djogi/src/transaction.rs:13-18, 207-309`). The capability is
  already implemented; only a caller-named convenience method is
  missing, and that is convenience-only. Recorded above as a deferral
  with anchor.
- Issue 3 (pgcrypto, now renumbered as issue 2) was reframed from
  "typed surface absent" to "expression-side typed wrapper missing —
  extension reachable via allowlist." Reviewer correctly identified
  `djogi/src/migrate/bootstrap.rs:139` (allowlist entry) and
  `djogi/src/testing.rs:834,1438` (test fixture references) showing
  the differ projects pgcrypto without an issue; only the SQL-side
  typed wrapper is the gap. The issue stays in the file-list with
  the reframed scope.

**Routing source:** `2026-05-10-cluster4-vs-postgres-coverage-xref.md`
"High-leverage gaps NOT currently in any v3 cluster" table.

## Amendment 5: Tighten Cluster 4D scope around spatial constructor breadth

**Section:** "Cluster 4D: Spatial Constructors And Alias Safety"
(`docs/superpowers/plans/2026-05-09-phase8-5-alpha-readiness-v3.md`
lines 426–434)

**Current text:**
> **Primary issues:** `djogi#72`, `djogi#71`.
>
> - [ ] Add the missing public typed constructor for
>       `SpatialExpr::Intersection` and verify it composes in typed spatial
>       filters/expressions.
> - [ ] Strengthen alias validation so user aliases cannot collide with model
>       columns, framework aliases, or aggregate/window aliases.

**Proposed text (add as a third bullet):**

> - [ ] Document spatial-constructor breadth as a v0.1.0 carve-out:
>       djogi v0.1.0 ships GeoPoint / Polygon / MultiPolygon /
>       LineString / MultiLineString / MultiPoint constructors plus
>       the relationship / measurement / overlay families. PostGIS
>       constructors not in this set (e.g. `ST_TileEnvelope`,
>       `ST_HexagonGrid`, `ST_SquareGrid`, `ST_Letters`,
>       `ST_MakePointM`, `ST_MakeValid`, `ST_IsValidDetail`,
>       `ST_IsValidReason`) are out-of-scope for v0.1.0. File one
>       tracking issue (`framework gap: extended PostGIS constructor
>       coverage`) with the explicit list and the anchor: "v0.1.0
>       spatial alpha covers the canonical typed surface; specialized
>       constructors land post-v0.1.0 when an adopter shape requires
>       them, with the raw-SQL bypass attribute as the interim
>       escape." If `ST_TileEnvelope` is required by 4C #92's
>       MVT/Geobuf row-shape work, escalate it from the tracking
>       issue into 4C scope.

**Rationale:** Catalog `02-postgis-functions.md` enumerates 400+
PostGIS functions; v0.1.0 covers ~30. Without an explicit carve-out
issue, the gap is implicit and unmeasurable. Filing one tracking
issue with the explicit list (rather than 370 individual issues)
keeps the post-v0.1.0 backlog manageable while satisfying the
"no arbitrary deferrals" rule.

**Routing source:** `docs/research/postgres-coverage/2026-05-09/02-postgis-functions.md`
lines 14–105 (constructors and accessors); spec-grep against
`djogi/src/geo/mod.rs` confirms the v0.1.0 surface.

## New issues to file (consolidated list, re-eval `2026-05-10`; GPT-5.5 corrections `2026-05-12`)

DO NOT actually file the issues. The user reviews first.

**Each entry below is a file-ready candidate.** Candidate 1 (MERGE)
carries the full body sketch at Amendment 1a above; candidates 2–7
carry inline Stage 1.5 closing-condition checklists with the same
five-bullet structure (rustdoc + doctest + spec amendment + live
PG18 test + adopter guide example), so the file-issues step can
copy them verbatim without re-derivation.

**Reviewer correction tally:**
- Was: 8 issues to file. Now: **7 issues to file**.
- One issue dropped from the file-list (SAVEPOINT — capability
  already exists via nested `atomic()`; ergonomics convenience-only).
- One issue reframed but kept in the file-list (pgcrypto — extension
  reachable; expression-side wrapper still missing).
- One amendment evaluation bullet absorbed by an existing issue
  (PG18 temporal constraints → `djogi#150`).
- Pre-existing GH issues confirmed in scope (no new issue needed):
  `djogi#150` (PG18 temporal constraints).

1. **`framework gap: MERGE INTO ... USING ... typed surface`**
   - Cluster: 4B (after Cluster 4.0 audit confirms alpha-blocking)
   - Body: see Amendment 1a above (full Stage 1.5 issue body sketch)
   - Closing-condition: Stage 1.5 format (rustdoc + doctest + spec
     amendment + live PG18 test + user guide example)
   - Re-eval status: net-new-valid; reviewer-confirmed (zero `MERGE
     INTO` in `djogi/src`).

2. **`framework gap: FTS configuration DDL (CREATE TEXT SEARCH
   CONFIGURATION / DICTIONARY / PARSER / TEMPLATE)`**
   - Cluster: post-v0.1.0
   - Body: anchor "tsvector/tsquery typed query surface is implemented
     at `djogi/src/expr/node.rs:398, 417, 431` and `djogi/src/fts.rs`;
     DDL is declarative low-frequency; raw bypass is acceptable"
   - Closing-condition (Stage 1.5 inline):
     - [ ] Rustdoc on the new `#[fts]` (or chosen name) attribute and any helper builders, with one full FTS-config worked example
     - [ ] Doctest exercises `CREATE TEXT SEARCH CONFIGURATION` minimum, plus at least one of `DICTIONARY` / `PARSER` / `TEMPLATE`
     - [ ] Spec amendment in `docs/spec/migrations.md` (extensions/DDL section) describes the descriptor surface and emitted SQL
     - [ ] Live PG18 integration test creates an FTS configuration via the typed surface and matches it against `to_tsvector(<cfg>, $1)` query results
     - [ ] Adopter guide example added under `docs/guide/` (likely `docs/guide/fts.md`) showing typed FTS-config use with a typed `tsvector` column
   - Re-eval status: net-new-valid; unchanged.

3. **`framework gap: pgcrypto expression-side typed wrapper
   (encrypt/decrypt/digest/hmac/gen_salt etc.) — extension itself IS
   reachable via the migration emitter allowlist`**
   - Cluster: post-v0.1.0
   - Body: anchor "extension allowlisted at
     `djogi/src/migrate/bootstrap.rs:139` and validated at line 821;
     test fixtures use it (`djogi/src/testing.rs:834, 1438`). Only
     the SQL-side typed wrapper for crypto operations is missing.
     Adopters use Rust-side crypto today (sha2, hmac, ring); pgcrypto
     is post-v0.1.0 only if a real adopter shape requires server-side
     crypto."
   - Closing-condition (Stage 1.5 inline):
     - [ ] Rustdoc on each typed wrapper (`encrypt`, `decrypt`, `digest`, `hmac`, `gen_salt`, `crypt`) with one full worked example per family
     - [ ] Doctest exercises a digest + hmac round trip plus an encrypt/decrypt round trip
     - [ ] Spec amendment in `docs/spec/queries.md` (or successor) names the new expression family and its result typing
     - [ ] Live PG18 integration test verifies emitted SQL against pgcrypto installed in a test cluster
     - [ ] Adopter guide example added under `docs/guide/` (likely `docs/guide/crypto.md`) clarifying when to prefer pgcrypto vs Rust-side crypto
   - **Re-eval correction (`2026-05-10`):** was framed as "extension
     absence" in v2. Reviewer correctly noted the extension IS
     reachable via the allowlist. Reframed as expression-side typed
     wrapper gap, NOT extension absence.

4. **`framework gap: PG18 scalar functions (uuidv7, uuidv4, array_sort,
   array_reverse, casefold, crc32, crc32c, gamma, lgamma)`**
   - Cluster: post-v0.1.0 (one umbrella issue)
   - Body: per-function anchor table; HeerId/RanjId obviates UUID
   - Closing-condition (Stage 1.5 inline, applied per individual
     function once a sub-issue is promoted):
     - [ ] Rustdoc on the typed wrapper(s) with at least one worked example
     - [ ] Doctest exercises the wrapper's result typing
     - [ ] Spec amendment names the wrapper in `docs/spec/queries.md` (or successor)
     - [ ] Live PG18 integration test verifies emitted SQL against PG18
     - [ ] Adopter guide example added under `docs/guide/` if the wrapper is non-obvious; otherwise the rustdoc example suffices
   - Re-eval status: net-new-valid; unchanged.

5. **`framework gap: MIN()/MAX() over arrays and composites (PG18)`**
   - Cluster: 4C tail (after #89 type-state lands) OR post-v0.1.0
   - Body: anchor "Kind discriminator from #89 may need a new variant
     for composite/array aggregates"
   - Closing-condition (Stage 1.5 inline):
     - [ ] Rustdoc on the new aggregate variant(s) (likely a new `AggregateExpr<Out, Kind>` Kind) with worked examples for both array and composite arguments
     - [ ] Doctest exercises `min_array_agg` / `max_array_agg` / `min_composite_agg` / `max_composite_agg` minimum set
     - [ ] Spec amendment in `docs/spec/queries.md` describes the aggregate-over-non-scalar typing rules
     - [ ] Live PG18 integration test runs the four aggregates against a real table
     - [ ] Adopter guide example added under `docs/guide/aggregates.md` (or successor) showing typical use
   - Re-eval status: net-new-valid; reviewer-confirmed not covered
     by existing #88 (which is the aggregate umbrella for the
     existing surface, not array/composite extension).

6. **`framework gap: extended PostGIS constructor coverage
   (TileEnvelope, HexagonGrid, SquareGrid, Letters, MakePointM,
   MakeValid, IsValidDetail, IsValidReason)`**
   - Cluster: post-v0.1.0 (one umbrella issue)
   - Body: explicit list with the v0.1.0 carve-out anchor
   - Closing-condition (Stage 1.5 inline, applied per individual
     constructor once a sub-issue is promoted):
     - [ ] Rustdoc on the typed constructor with one full worked example
     - [ ] Doctest exercises the constructor's result-geometry typing
     - [ ] Spec amendment in `docs/spec/queries.md` (spatial section) names the constructor and its result-typing semantics
     - [ ] Live PG18 + PostGIS integration test verifies the emitted SQL against a real cluster
     - [ ] Adopter guide example added under `docs/guide/spatial.md` (or successor) if the constructor's use case is non-obvious
   - Re-eval status: net-new-valid; reviewer-confirmed no umbrella
     constructor-breadth issue exists.

7. **`framework gap: PG18 OLD / NEW in RETURNING for INSERT / UPDATE /
   DELETE / MERGE`**
   - Cluster: 4B as #4B.8 if Cluster 4.0 audit confirms alpha-blocking
   - Body: covers audit/event-publication shapes where pre and post
     image are both needed in one round trip
   - Closing-condition (Stage 1.5 inline):
     - [ ] Rustdoc on the `RETURNING (OLD ..., NEW ...)` typed surface (likely a new builder on `QuerySet::update` / `delete` / `merge_into`)
     - [ ] Doctest exercises returning OLD + NEW pair for at least one UPDATE
     - [ ] Spec amendment in `docs/spec/queries.md` (or successor) names the surface and the PG18-version-gating
     - [ ] Live PG18 integration test verifies emitted SQL for all four shapes (INSERT / UPDATE / DELETE / MERGE)
     - [ ] Adopter guide example added under `docs/guide/audit.md` (or successor) showing audit/event-publication use
   - Re-eval status: net-new-valid; unchanged.

## Issues NOT filed (anchored deferrals, re-eval `2026-05-10`)

The following candidate from the v2 draft was deliberately NOT filed:

- **`framework gap: caller-named savepoint(name) ergonomics on top of
  nested atomic() (which IS the typed savepoint surface)`** — anchored
  deferral. Capability already exists at
  `djogi/src/transaction.rs:13-18, 207-309` (nested `atomic()`
  pushes `SAVEPOINT sp_<depth>` at line 223-227, releases on `Ok` at
  line 265, rolls back on `Err` at line 281). Caller-supplied opaque
  name is convenience-only. File lazily if/when an adopter shape
  demonstrates the need. Reviewer-confirmed no capability gap exists.

## Pre-existing GH issues that absorb earlier draft amendments

The following items from the v2 draft are NOT new issues — they
duplicate already-filed GH issues. Routing only:

- **PG18 temporal constraints (`WITHOUT OVERLAPS`, PERIOD FK,
  `NOT ENFORCED`, named NOT NULL)** → already filed as **`djogi#150`**.
  Route through Cluster 4.0 audit; no separate evaluation issue.
  Anchor: anchored deferral via existing GH issue.
