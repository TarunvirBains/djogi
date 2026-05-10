# Proposed Cluster 4 v3 Amendments

> Derived from `2026-05-10-cluster4-vs-postgres-coverage-xref.md`.
> Apply via inline edits to `docs/superpowers/plans/2026-05-09-phase8-5-alpha-readiness-v3.md` after independent review.

## Amendment 1: Add MERGE to Cluster 4B

**Section:** "Cluster 4B: Set And Relational SQL Shapes"
(`docs/superpowers/plans/2026-05-09-phase8-5-alpha-readiness-v3.md`
lines 392–406)

**Current text:**
> **Primary issues:** `djogi#101`, `djogi#102`, `djogi#103`, `djogi#104`,
> `djogi#105`, `djogi#106`.
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

**Proposed text:**
> **Primary issues:** `djogi#101`, `djogi#102`, `djogi#103`, `djogi#104`,
> `djogi#105`, `djogi#106`, `djogi#new-merge` (filed during Cluster 4.0
> audit; see Amendment 1a).
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

Cluster 4.0 Postgres 18+ feature gap audit
(`docs/superpowers/notes/2026-05-10-cluster4-vs-postgres-coverage-xref.md`).
The MASTER-CATALOG marks MERGE as `partial`; spec-grep confirms no
typed surface. Adopters who need MERGE today fall back to raw SQL.

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
- [ ] Spec amendment in `docs/spec/queryset.md` (or successor) describes
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
> - [ ] Evaluate the PG18 temporal-constraint family
>       (`WITHOUT OVERLAPS`, PERIOD FK, NOT ENFORCED, named NOT NULL)
>       and route as: `Implemented` (none today), `Roadmapped`
>       (WITHOUT OVERLAPS / PERIOD FK fold into 4E #148 since the use
>       case overlaps EXCLUDE on tstzrange), or filed as a new
>       post-v0.1.0 issue (NOT ENFORCED is unusual enough to warrant
>       a separate decision). PG18 is the supported floor per
>       `docs/spec/decisions.md`, so PG18-syntax forms should be
>       preferred over backport-equivalent forms when both work.
> - [ ] Tabulate the eight quick-wins from
>       `2026-05-10-cluster4-vs-postgres-coverage-xref.md` and flip
>       their catalog disposition from `unknown` to `Implemented`
>       on first audit pass — they have already been spec-grep
>       verified.

**Rationale:** The Cluster 4.0 contract today is "run the audit; route
alpha-blocking gaps." The cross-reference surfaced two PG18-specific
items (OLD/NEW RETURNING, temporal-constraint family) that the existing
4A-4E task lists do not name explicitly. Adding them as evaluation tasks
ensures the audit can route them rather than silently miss them. The
quick-wins tabulation prevents repeat spec-grep work.

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

> - [ ] Before closing Cluster 4.0, file the following five tracking
>       issues so the post-v0.1.0 backlog has them on the books and the
>       no-arbitrary-deferrals rule is satisfied:
>       1. `framework gap: SAVEPOINT / RELEASE SAVEPOINT / ROLLBACK TO
>          SAVEPOINT typed transaction surface` — anchor:
>          `atomic` + `retry_on_conflict` covers the dominant case;
>          nested savepoint is rare in adopter code.
>       2. `framework gap: FTS configuration DDL (CREATE TEXT SEARCH
>          CONFIGURATION / DICTIONARY / PARSER / TEMPLATE)` — anchor:
>          declarative low-frequency surface; `tsvector` / `tsquery`
>          / `to_tsvector` / `to_tsquery` typed query surface IS
>          implemented; raw-SQL bypass attribute acceptable for the
>          DDL during v0.1.0.
>       3. `framework gap: pgcrypto (encrypt/digest/hmac) typed
>          surface` — anchor: adopters use Rust-side crypto today
>          (sha2, hmac, ring); pgcrypto is post-v0.1.0 only if a real
>          adopter shape requires server-side crypto.
>       4. `framework gap: PG18 scalar functions (uuidv7, uuidv4,
>          array_sort, array_reverse, casefold, crc32, crc32c, gamma,
>          lgamma)` — anchor: HeerId/RanjId obviates UUID surface;
>          remaining scalars are roadmapped only when an adopter
>          shape needs them.
>       5. `framework gap: MIN()/MAX() over arrays and composites
>          (PG18)` — anchor: route as 4C tail-task after #89
>          type-state migration lands the Kind discriminator; until
>          then, raw SQL bypass is acceptable.

**Rationale:** The cross-reference identified five categorical gaps
that none of 4A-4E covers. Filing tracking issues during the audit
satisfies the "no arbitrary deferrals" rule (each has an explicit
anchor) and gives the post-v0.1.0 backlog a known shape rather than
discovery-by-customer-bug.

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

## New issues to file (consolidated list)

DO NOT actually file the issues. The user reviews first.

1. **`framework gap: MERGE INTO ... USING ... typed surface`**
   - Cluster: 4B (after Cluster 4.0 audit confirms alpha-blocking)
   - Body: see Amendment 1a above
   - Closing-condition: Stage 1.5 format (rustdoc + doctest + spec
     amendment + live PG18 test + user guide example)

2. **`framework gap: SAVEPOINT / RELEASE SAVEPOINT / ROLLBACK TO
   SAVEPOINT typed transaction surface`**
   - Cluster: post-v0.1.0
   - Body: anchor "atomic + retry_on_conflict covers the dominant case"
   - Closing-condition: Stage 1.5 format if/when adopter shape requires

3. **`framework gap: FTS configuration DDL (CREATE TEXT SEARCH
   CONFIGURATION / DICTIONARY / PARSER / TEMPLATE)`**
   - Cluster: post-v0.1.0
   - Body: anchor "tsvector/tsquery typed query surface is implemented;
     DDL is declarative low-frequency; raw bypass is acceptable"
   - Closing-condition: Stage 1.5 format if/when adopter shape requires

4. **`framework gap: pgcrypto (encrypt/digest/hmac) typed surface`**
   - Cluster: post-v0.1.0
   - Body: anchor "adopters use Rust-side crypto today; pgcrypto is
     server-side and only needed for specific shapes"
   - Closing-condition: Stage 1.5 format if/when adopter shape requires

5. **`framework gap: PG18 scalar functions (uuidv7, uuidv4, array_sort,
   array_reverse, casefold, crc32, crc32c, gamma, lgamma)`**
   - Cluster: post-v0.1.0 (one umbrella issue)
   - Body: per-function anchor table; HeerId/RanjId obviates UUID
   - Closing-condition: track as a list; promote individual entries to
     dedicated issues only when an adopter shape requires

6. **`framework gap: MIN()/MAX() over arrays and composites (PG18)`**
   - Cluster: 4C tail (after #89 type-state lands) OR post-v0.1.0
   - Body: anchor "Kind discriminator from #89 may need a new variant
     for composite/array aggregates"
   - Closing-condition: Stage 1.5 format

7. **`framework gap: extended PostGIS constructor coverage
   (TileEnvelope, HexagonGrid, SquareGrid, Letters, MakePointM,
   MakeValid, IsValidDetail, IsValidReason)`**
   - Cluster: post-v0.1.0 (one umbrella issue)
   - Body: explicit list with the v0.1.0 carve-out anchor
   - Closing-condition: track as a list; escalate to v3 4C if
     `ST_TileEnvelope` becomes required for MVT/Geobuf

8. **`framework gap: PG18 OLD / NEW in RETURNING for INSERT / UPDATE /
   DELETE / MERGE`**
   - Cluster: 4B as #4B.8 if Cluster 4.0 audit confirms alpha-blocking
   - Body: covers audit/event-publication shapes where pre and post
     image are both needed in one round trip
   - Closing-condition: Stage 1.5 format
