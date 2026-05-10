# Cluster 4 vs PG18+PostGIS Coverage Cross-Reference

> Prep work for Cluster 4.0 (Postgres 18+ feature gap audit, Area 19).
> Self-review only — independent GPT-5.5 xhigh review pending.

## v3 framing context (re-eval `2026-05-10`)

Per user clarification, **most catalog rows in
`docs/research/postgres-coverage/2026-05-09/` are intentionally
future-phase pointers** — the postgres-coverage research is a multi-phase
roadmap input, not a Phase 8.5 delivery checklist. The user has already
triaged the Phase 8.5-eligible subset into filed GH issues (#147, #148,
#150, plus the round-1 typed-surface gap set #99/#101–#106 and the
spatial/aggregate set #71/#72/#88/#89/#92/#94/#95).

This xref's job, post-correction, is narrow:

1. Surface gaps that are **alpha-blocking** AND **not yet covered** by
   v3 4A–4E AND **not yet filed** as a GH issue.
2. Confirm the existing 4A–4E routing matches what the catalog
   enumerates, flagging cases where the catalog mis-classifies a feature
   as `partial` when grep proves it is `unknown` (or vice versa).
3. Tabulate quick-wins where the catalog says `unknown` but spec-grep
   shows the feature IS implemented — these flip to `Implemented` on
   the audit ledger without further verification.

It is NOT a comprehensive triage of the ~850 `unknown` rows. The
Cluster 4.0 audit owns that work. This xref pre-stages it.

### Review history footer

- Initial deliverables landed at commit `ca93e9c` (3 docs, this one
  being one of them).
- careful-coder Opus reviewer ran a corrections-pass; verdict
  `ALLOW_WITH_CORRECTIONS`. The reviewer caught:
  - Amendment 4 issue 1 (SAVEPOINT) framed as absence; in fact nested
    `atomic()` IS the typed surface (`djogi/src/transaction.rs:14-18,
    207-309`). Reframed in this re-eval as ergonomics-on-top, not
    capability gap.
  - Amendment 2 (temporal-constraint bullet) duplicates already-filed
    `djogi#150`. Reframed as "route #150 through 4.0 audit" rather
    than as net-new evaluation work.
  - pgcrypto entry conflated extension reachability (allowlisted at
    `djogi/src/migrate/bootstrap.rs:139`) with expression-side typed
    wrapper absence. Reframed in this re-eval.
- This re-eval (`2026-05-10`, post-corrections) addresses those three
  reframings and adds this v3 framing context section. No new
  substantive amendments.

## Method

Read v3 plan Cluster 4 sections 4.0–4E end-to-end
(`docs/superpowers/plans/2026-05-09-phase8-5-alpha-readiness-v3.md` lines
356–476). Read all six PG18+PostGIS catalog files
(`docs/research/postgres-coverage/2026-05-09/01..06`) plus `MASTER-CATALOG.md`
and `RESEARCH-SUMMARY.txt`. Read Area 19 contract in
`docs/superpowers/red-team-gate-plan.md` lines 163–188. Sample-grepped
`djogi/src/expr/`, `djogi/src/query/`, `djogi/src/migrate/`, `djogi/src/jsonb/`,
`djogi/src/geo/`, and `djogi/src/fts.rs` for the highest-leverage
"unknown" rows so the audit ledger can flip them without a separate pass.

Coverage labels follow the catalog's three-bucket scheme
(`covered` / `partial` / `deferred`). The fourth bucket (`unknown`) is what
this xref triages — every Cluster 4 subcluster maps onto a slice of the
~850-row "unknown" set.

## Per-subcluster findings

### Cluster 4A: Pair-Tuple, Mating Pairs, Punnu Showcase

**v3 scope:** djogi#99 (typed pair-tuple), #84 (multi-model joins), #108
(Punnu wrap), #85 (mating-pairs follow-ups). Tasks: design
`JoinedQuerySet<T,U>` with `annotate`/`qualify`/`partition_by`; cover
self-join, two-model cross-join, closure-self-join; retrofit mating-pairs
Step 3; replace cross-herd binary-territory fallback with typed pair-tuple
spatial expression; show Punnu-wrapped typed query path; close #85
age-compatibility multiplier and bench fixture.

**Catalog overlap:**
- `03-pg18-sql-reference.md` lines 11–18: `SELECT` is `covered` core, but
  multi-table SELECT through `JoinedQuerySet` is implicit in the catalog's
  "covered" bucket without being explicitly enumerated. The catalog tracks
  SQL commands not query-API shapes, so 4A's main shape (the typed
  pair-tuple) is invisible to the catalog.
- `02-postgis-functions.md` lines 252–267: `ST_Intersection` and
  `ST_Area` are both marked `covered`. Spec-grep shows
  `SpatialExpr::Intersection` (djogi/src/expr/spatial.rs line 234) is
  NOT publicly constructible — the constructor is gated behind the
  `area_of_intersection` fused shape because there is no
  `Expr<Polygon>` / geometry-typed Expr (rustdoc on `Intersection` lines
  223–232 explicitly says "no public typed constructor today"). This is
  the gap djogi#72 already files.

**Gaps surfaced by catalog NOT in v3 4A:**
- None new. The catalog under-enumerates the pair-tuple shape, so no
  surprise gaps surface here. djogi#72 (Intersection constructor) is
  already routed to Cluster 4D, not 4A — that routing is correct because
  the constructor surface is single-pair-spatial, not pair-tuple-shape.

**v3 4A items NOT supported by catalog:**
- The pair-tuple typed surface itself (`JoinedQuerySet<T,U>`) is a
  Djogi-API concept the catalog doesn't track — this is fine, the catalog
  is PG-surface-only.

**Routing recommendation:** Keep 4A as-is. The catalog's silence on
multi-model query shape is expected and not a routing signal. **One
amendment opportunity:** add to 4A a sub-task to surface
`SpatialExpr::Intersection`'s typed constructor whenever the pair-tuple
surface is expressive enough to hold a geometry-typed `Expr<Polygon>`
intermediate. Today that's blocked by no `Expr<G>` for geometry types
(spatial.rs line 246–247). If 4A delivers `JoinedQuerySet<T,U>`, the
follow-on question of "does pair-tuple unblock typed geometry-typed Expr?"
should be evaluated mid-cluster, not deferred.

### Cluster 4B: Set And Relational SQL Shapes

**v3 scope:** djogi#101 (UNION/INTERSECT/EXCEPT), #102 (LATERAL),
#103 (typed VALUES), #104 (FOR SHARE), #105 (CHECK constraint via
`#[field()]`), #106 (`INSERT ... SELECT`).

**Catalog overlap:**
- `03-pg18-sql-reference.md` line 16: `MERGE` is marked `partial` (PG15+
  conditional DML). Grep against `djogi/src/` confirms NO `MERGE`
  surface — only one comment in `migrate/diff.rs:572` that uses the word
  "MERGES" in a different sense (cluster of cross-flipping options).
  The catalog's `partial` claim is **wrong by my grep**: MERGE is
  `unknown`/uncovered, not `partial`. There is no MERGE typed surface.
- `03-pg18-sql-reference.md` lines 12, 17: `INSERT` is `covered`, `VALUES`
  is `unknown`. Grep confirms `INSERT` works through `Model::create` /
  `Model::save`; standalone `VALUES` clause is not exposed (only as
  internal lateral codegen in `query/closure.rs` and
  `query/recursive.rs`). v3 4B #103 correctly identifies this gap.
- `03-pg18-sql-reference.md` line 18: `SELECT INTO` is `unknown`. Grep
  shows zero matches — no public surface. v3 4B does not list this. Not
  a blocker (modern Postgres style prefers `CREATE TABLE AS`).
- `03-pg18-sql-reference.md` lines 56–63: `EXCLUDE`, `DEFERRABLE`,
  `NOT ENFORCED`, `TEMPORAL`, `PERIOD FK` all `unknown`. Grep against
  `migrate/`: the migration emitter writes `DEFERRABLE INITIALLY DEFERRED`
  in PK-flip fixtures (`pk_flip_emitter_output_section_8.sql`), so the
  composer can emit deferrable FKs. But `#[field()]` macro surface
  exposes none of these. v3 4E covers EXCLUDE via #148; 4B covers CHECK
  via #105; the others are uncovered.
- `03-pg18-sql-reference.md` lines 60–63 — **PG18-new** constraint
  forms (NOT ENFORCED, WITHOUT OVERLAPS, PERIOD FK) are exactly the
  Area 19 audit material. v3 4B does not address them.
- `03-pg18-sql-reference.md` lines 110–116: SAVEPOINT family `unknown`.
  **Re-eval correction (post-Opus review):** SAVEPOINT IS implemented
  as the typed surface — it just isn't a *named* `savepoint(name)` API.
  Nested `atomic(&mut *outer, |inner| ...)` pushes a savepoint
  (`SAVEPOINT sp_<depth>` at `djogi/src/transaction.rs:223-227`),
  releases on `Ok` (`RELEASE SAVEPOINT` at line 265), rolls back on
  `Err` (`ROLLBACK TO SAVEPOINT` at line 281), and rolls back on panic
  (line 300). Public API is `crate::transaction::atomic`, with
  rustdoc explicitly noting "Nested calls push a Postgres savepoint
  rather than opening a new transaction" (transaction.rs:13-18). This
  is a v0.1.0 **ergonomics gap** (no opaque-name savepoint method) —
  NOT a capability gap. The audit ledger should mark SAVEPOINT as
  `partial` (anonymous-depth-named savepoints via nested atomic;
  no caller-supplied name surface).

**Gaps surfaced by catalog NOT in v3 4B:**
1. **`MERGE` (PG15+)**: catalog mis-classifies as `partial`; my grep
   says `unknown`. No typed surface. **Routing:** add to v3 4B as
   #4B.7 OR file a new issue and route to a follow-up cluster. Consensus
   choice: file as a new issue (`djogi#new-merge`), route to v3 4B with
   note that the typed shape needs `WHEN MATCHED` / `WHEN NOT MATCHED`
   condition wiring.
2. **PG18 `OLD`/`NEW` in `RETURNING`** (catalog row 59
   `01-pg18-release-notes.md`): UPDATE/DELETE/INSERT/MERGE can return
   both old and new row shapes. djogi has no `RETURNING` typed surface
   exposed (grep confirms zero matches). **Routing:** this is an Area
   19 PG18-new item; route to Cluster 4.0's gap audit, then to
   Cluster 4B as #4B.8 if the audit confirms it as alpha-blocking.
3. **PG18 temporal constraints (`WITHOUT OVERLAPS`, PERIOD FK,
   `NOT ENFORCED`, named NOT NULL)** —
   exactly the use case for `tstzrange` exclusion that #148 already
   targets. The PG18 syntax is a more declarative form (`PRIMARY KEY
   (id, validity WITHOUT OVERLAPS)`). **Re-eval correction
   (post-Opus review):** This is already filed as `djogi#150`
   (`framework gap: PG18 temporal constraints (WITHOUT OVERLAPS,
   PERIOD FK, NOT ENFORCED)`). #150's body explicitly covers all four
   PG18 temporal-constraint primitives plus the design coordination
   with #148 ("Sibling of #148 — same no-overlap motivation, distinct
   DDL surface area"). **Routing:** route #150 through the 4.0 audit;
   no new evaluation issue needed. Amendment 3 (4E #148 PG18-syntax
   preference note) still stands as a follow-up clarification because
   #148's task wording targets EXCLUDE form exclusively while #150's
   shape is the more declarative PG18 emission.

**v3 4B items NOT supported by catalog:** None — every 4B item maps to
a catalog row.

**Routing recommendation:** Amend 4B to add MERGE
(catalog mis-classified as `partial`; actually uncovered) and to route
PG18 `OLD`/`NEW RETURNING` through the 4.0 gap audit before deciding
its cluster home. SELECT INTO can defer past v0.1.0 with the standard
"named API surface, no live adopter demand" anchor. SAVEPOINT does
NOT defer — it's already implemented as nested `atomic()` (see
correction above); the only remaining work is an optional
ergonomics-on-top issue if a caller-supplied savepoint name is needed,
which is a v0.1.0+ enhancement not a v0.1.0 blocker. PG18 temporal
constraints are already filed as `djogi#150` (no new issue required).

### Cluster 4C: Aggregate, Window, And Row-Shape IR

**v3 scope:** djogi#88 (aggregate coverage), #89 (`AggregateExpr<Out,
Kind>` type-state — `ValueAgg` / `MetadataAgg` / `OrderedSetAgg` /
`HypotheticalSetAgg`), #92 (row-shape IR for `ST_AsMVT` / `ST_AsGeobuf`),
#94 (variadic `GROUPING(c1, ..., cN)`), #95 (f64
qualify/window-fraction).

**Catalog overlap:**
- `02-postgis-functions.md` lines 168–169: `ST_AsMVT` and `ST_AsGeobuf`
  are `unknown`. Grep confirms zero matches in `djogi/src` — fully
  uncovered. v3 4C #92 correctly targets this; the row-shape IR work
  (per djogi#92 issue body) is the prerequisite, MVT/Geobuf are the
  consumers. Aligned.
- `01-pg18-release-notes.md` lines 32–35: `min_array_aggregate`,
  `max_array_aggregate`, `min_composite_aggregate`,
  `max_composite_aggregate` are PG18 new aggregates over arrays /
  composites. Grep against `expr/aggregate.rs`: no array/composite-shape
  aggregate. These are scalar-aggregate form, not row-shape — they sit
  inside the existing `AggregateExpr<Out>` template once `Out` can be
  `Vec<T>` or composite. Not in v3 4C, but they ARE PG18-new and Area
  19-relevant. **Routing:** route through 4.0 audit; if alpha-blocking
  add as a #4C.6 sub-task once #89 type-state is in (since composite
  aggregates may need a different `Kind`).
- `01-pg18-release-notes.md` line 87: `query_id_jumbling_optimization`
  and similar EXPLAIN/optimizer changes are GUC-level; out of djogi
  scope per Area 19 carve-out rules.
- `06-operator-classes.md` line 105: `gin_jsonb_path_ops` is `unknown`
  — JSONB path-value GIN opclass. Grep against `migrate/`: index
  creation supports `using = "..."` but no typed path-ops opclass
  attribute. This is index-projection territory, not aggregate; route
  to Cluster 4E #148 if GIN-on-JSONB-paths is needed for trigram-
  adjacent shapes, or defer to post-v0.1.0 with explicit anchor.

**Gaps surfaced by catalog NOT in v3 4C:**
- **PG18 `MIN`/`MAX` over arrays and composites**: route through 4.0
  audit; if alpha-blocking add as 4C tail-task after #89 type-state.
- **PG18 array operations `array_sort()` and `array_reverse()`**
  (catalog rows 25–26): scalar functions, not aggregates. Not 4C scope;
  route to 4.0 audit for likely roadmapped/post-v0.1.0 disposition.

**v3 4C items NOT supported by catalog:**
- The `AggregateExpr<Out, Kind>` type-state migration (#89) is a
  Djogi-API concept the catalog doesn't track — fine.

**Routing recommendation:** Keep 4C as-is. The PG18 array/composite
aggregates are real but should ride the 4.0 audit's roadmapped/
out-of-scope decision rather than landing automatically in 4C.

### Cluster 4D: Spatial Constructors And Alias Safety

**v3 scope:** djogi#72 (typed `SpatialExpr::Intersection` constructor),
#71 (alias collision validation).

**Catalog overlap:**
- `02-postgis-functions.md` line 253: `ST_Intersection` is `covered`.
  Spec-grep confirms the SQL is emitted (spatial.rs line 411) but the
  variant is `#[allow(dead_code)]` and has no public constructor —
  exactly the gap #72 fixes. Catalog "covered" overstates the public
  surface; spec-grep is more accurate.
- `02-postgis-functions.md` line 174 onwards: 25+ spatial operators,
  most `unknown`. Grep against `expr/spatial.rs`: only the `&&`-style
  bbox operators are exposed via `BoundedBy`; no separate typed access
  to `&&&` (3D bbox), `<<`, `>>`, `<#>` (bbox distance), `<<->>` (N-D
  distance). **Not in v3 4D.** Routing: defer to post-v0.1.0 unless an
  adopter shape needs them. Anchor: "no current adopter shape; spatial
  alpha covers 2D bbox + relationships + measurements + overlay; rare
  ops can land in a follow-up." File as a placeholder issue if any
  surface forensically.

**Gaps surfaced by catalog NOT in v3 4D:**
- **Geometry constructor breadth** (catalog 02-postgis-functions.md
  lines 14–26): `ST_MakePointM`, `ST_MakePolygon` from rings,
  `ST_TileEnvelope`, `ST_HexagonGrid`, `ST_SquareGrid`, `ST_Letters`
  all `unknown`. Grep against `geo/`: only the basic geometry types
  exist (Point/MultiPoint/Polygon/MultiPolygon/LineString/MultiLineString
  per `mod.rs`). **Not in v3 4D.** Most are specialized — defer to
  post-v0.1.0 with the "spatial alpha covers core constructors" anchor;
  file `ST_TileEnvelope` separately if vector-tile work in 4C #92
  needs it as a precondition.
- **`ST_MakeValid` / `ST_IsValidDetail` / `ST_IsValidReason`** (catalog
  lines 109–111): validation depth. `ST_IsValid` is `covered`; the
  detail variants are `unknown`. Routing: file as a low-priority
  post-v0.1.0 issue; spatial alpha doesn't need them.
- **Geography vs geometry mode toggle** — the ST_Intersection variant
  exists only as geometry (no geography overload in PostGIS 3.x per
  spatial.rs comment line 407–409). This is a PostGIS limit, not a
  Djogi gap; the AreaOfIntersection composed shape already handles the
  workaround.

**v3 4D items NOT supported by catalog:**
- Alias safety (#71) is a Djogi-API concern; catalog doesn't track.

**Routing recommendation:** Keep 4D as-is. The catalog's "covered"
classification of `ST_Intersection` is an under-specification — the SQL
emitter has it, but no adopter can reach it; #72 fixes that. The
broader spatial-constructor breadth (TileEnvelope, HexagonGrid,
ST_MakeValid) is post-v0.1.0 with anchored deferrals.

### Cluster 4E: Postgres Text Search And Constraint Coverage

**v3 scope:** djogi#147 (typed `pg_trgm` surface — extension dep,
similarity expression, `%` predicate, `gin_trgm_ops`/`gist_trgm_ops`
opclass migration), #148 (`btree_gist` exclusion constraints, range
fields `tsrange`/`tstzrange`/`daterange`/integer ranges as needed for
exclusion).

**Catalog overlap:**
- `04-extensions.md` lines 15–16, 43: `pg_trgm` and `btree_gist` /
  `btree_gin` all `deferred` (filed as #147 / #148). Catalog and v3
  agree.
- `06-operator-classes.md` line 146–147: `gin_btree_ops` and
  `gist_btree_ops` are `deferred` (#148). Aligned.
- `06-operator-classes.md` lines 99–106: GIN opclass family. Grep
  against `migrate/bootstrap.rs`: `pg_trgm` is in the extension
  allowlist (line 138), and `btree_gist` is in the allowlist
  (line 137). The migration emitter projects extension dependencies.
  Spec-grep against `expr/`: trigram similarity expression has zero
  matches — fully uncovered as a typed expression. v3 #147 task list
  is exactly right.
- `04-extensions.md` line 24: `fuzzystrmatch` (Levenshtein, Soundex,
  Metaphone) is `unknown`. Grep against `djogi/src`: zero matches.
  **Not in v3 4E.** Routing: defer post-v0.1.0; #147 trigram support
  covers the dominant fuzzy-match use case. Anchor: "fuzzystrmatch is
  a less-used legacy extension; trigram covers the main fuzzy-match
  shape; no current adopter need."
- `02-postgis-functions.md` lines 86–87: `to_tsvector`, `to_tsquery`,
  `ts_rank`, `ts_rank_cd`. Grep against `expr/node.rs` and
  `expr/sql.rs`: TsMatch/TsRank/TsRankCd ARE implemented (node.rs
  lines 387–438). The catalog under-classifies these as `unknown`
  by category — they're really `covered` for FTS basics. **Quick win
  candidate.**
- `03-pg18-sql-reference.md` lines 244–256: 12 `CREATE/ALTER/DROP TEXT
  SEARCH ...` commands all `unknown`. Grep confirms zero — no typed
  surface for FTS configuration / dictionary / parser / template. v3
  4E #147 task body says "position trigram search as distinct from
  FTS rather than a replacement for it." Aligned. The FTS-config DDL
  is post-v0.1.0 with anchor: "djogi exposes typed `TsMatch`/`TsRank`
  on `tsvector` columns; FTS config / dictionary / parser DDL is a
  declarative, low-frequency surface; raw-SQL escape hatch acceptable
  during v0.1.0 with the bypass attribute." File a tracking issue
  before publish if not already filed.

**Gaps surfaced by catalog NOT in v3 4E:**
1. **FTS configuration DDL** (CREATE TEXT SEARCH CONFIG/DICT/PARSER/
   TEMPLATE): file as post-v0.1.0 issue with anchor above.
2. **`fuzzystrmatch` extension**: defer post-v0.1.0; trigram covers
   the main shape.
3. **PG18 Estonian FTS stemming** (catalog row 29
   `01-pg18-release-notes.md`): GUC-level / locale-level; out of
   djogi scope per Area 19 carve-out rules.
4. **PG18 `casefold()` function** (catalog row 31): scalar function;
   route to 4.0 audit. citext already covers the dominant
   case-insensitive use case.

**v3 4E items NOT supported by catalog:** None.

**Routing recommendation:** Keep 4E as-is for #147/#148. The FTS
configuration DDL gap should be filed as a tracking issue with explicit
post-v0.1.0 routing and the "raw-SQL bypass attribute is acceptable for
this surface during v0.1.0" anchor.

## Cluster 4.0 (Area 19 Postgres 18+ gap audit) — what the catalog already gives us

Area 19 (red-team-gate-plan.md lines 163–188) requires three
dispositions per PG18 feature: **Implemented**, **Roadmapped**, **Out of
scope**. The catalog already pre-classifies into four buckets
(`covered` / `partial` / `deferred` / `unknown`). The mapping is:

| Catalog bucket | Area 19 disposition | Notes |
|---|---|---|
| `covered` | `Implemented` | Cite the spec-grep evidence in the audit ledger; ~100 surfaces. |
| `partial` | Mostly `Implemented` with caveats; some `Roadmapped` | ~15 surfaces; each needs a one-liner specifying what's covered vs gap. |
| `deferred` | `Roadmapped` (already filed) | ~4 surfaces, all already on Cluster 4 task list. |
| `unknown` | Triage required | ~850; this is the bulk audit work. |

**The 4.0 audit's main work is converting the ~850 `unknown` rows into
Implemented / Roadmapped / Out-of-scope dispositions.** From sample-grep
findings above, I estimate the breakdown is roughly:

- **~100–150 `unknown` rows are actually `Implemented`** (catalog grep
  was light; e.g. FTS `to_tsvector`/`to_tsquery`/`ts_rank` family,
  Postgres POSIX `~`/`~*` regex via `Lookup::Regex`, `select_for_update`
  / `nowait` / `skip_locked`, `ON CONFLICT (...) DO UPDATE`, JSONB
  containment via path operators, etc.). See "Quick-wins" below.
- **~150–250 `unknown` rows are `Roadmapped`** (already covered by
  Cluster 4 issues, or close-cousin issues that should join Cluster 4 by
  amendment): MERGE, set ops, LATERAL, VALUES, FOR SHARE, CHECK,
  INSERT-SELECT, pair-tuple shapes, MVT/Geobuf, type-state aggregates,
  GROUPING bitmask, qualify f64, ST_Intersection constructor, alias
  safety, pg_trgm, btree_gist exclusion.
- **~450–600 `unknown` rows are `Out of scope`**: replication
  topology, FDW DDL, TABLESPACE, role/auth DDL, FTS dictionary/parser
  DDL, event triggers, security labels, cursor management, prepared
  statements (driver-level not query-API level), libpq protocol
  parameters, psql CLI features, monitoring GUCs, pg_upgrade flags,
  pg_dump flags, vacuum scheduling, OAuth method, SSL configuration,
  many specialized PostGIS functions (clustering, coverage, trajectory,
  exotic I/O like FlatGeobuf/MARC21/TWKB).

**MASTER-CATALOG.md lines 127–169 — "Notable Clusters for Orchestrator
Triage" — six clusters mapped to v3:**

1. **PG18 Constraint & Temporal Features** — overlap with v3 4B #105
   (CHECK) and v3 4E #148 (EXCLUDE/range). PG18-specific syntax
   (WITHOUT OVERLAPS, PERIOD FK, NOT ENFORCED, named NOT NULL) is
   PG18-new and Area 19-targeted. **Routing decision:** the audit
   should classify each as Implemented (none today), Roadmapped (the
   ones #148 covers), or Out-of-scope. PG18 NOT ENFORCED is unusual
   enough that it should be a separate issue; PERIOD FK / WITHOUT
   OVERLAPS go into #148's range/exclusion family.
2. **PG18 I/O & Performance GUCs** — explicitly out-of-djogi-scope per
   Area 19 carve-out rules. Document in `docs/spec/scope.md`.
3. **PostGIS Advanced Geometry Operations** — clustering, coverage,
   trajectory, etc. Mostly out-of-scope for v0.1.0 spatial alpha.
   Anchor: "spatial alpha covers GeoPoint + relationship + measurement
   + overlay families; advanced geometry/trajectory/clustering is
   post-v0.1.0."
4. **System Catalog Introspection** — pg_backend_memory_contexts,
   pg_stat_io. Out-of-scope for query-API; introspection happens
   through `pg_class`/`pg_attribute` for descriptor work, which IS
   covered.
5. **PostgreSQL 18 New Functions** — uuidv7/v4 (Area 19 priority;
   verify if HeeRanjId obviates need), array_sort/reverse,
   crc32/crc32c, gamma/lgamma, casefold. Each needs a per-function
   audit decision; most likely Roadmapped (file `pg-18-scalars`
   tracking issue) or Out-of-scope.
6. **Replication & Logical Decoding** — out-of-scope; file as
   `docs/spec/scope.md` carve-out.

## High-leverage gaps NOT currently in any v3 cluster

| Gap | Catalog row | Alpha-blocking? | Suggested routing |
|---|---|---|---|
| `MERGE` (PG15+ conditional DML) | `03-pg18-sql-reference.md` line 16 | **Likely yes** — UPSERT-with-conditions is a real shape; catalog mis-classifies as `partial`, grep confirms uncovered | New issue; route to v3 4B as #4B.7 |
| PG18 `OLD`/`NEW` in `RETURNING` | `01-pg18-release-notes.md` line 59 | Possibly — adopter use case is audit/event publication where you want both pre and post images | Route to 4.0 audit; if alpha-blocking add to 4B as #4B.8 |
| PG18 `WITHOUT OVERLAPS` / PERIOD FK / `NOT ENFORCED` / named NOT NULL — **already filed as `djogi#150`** | `01-pg18-release-notes.md` lines 49–51 + `djogi#150` body | Yes — #150 marks "Required for Phase 8.5 alpha-readiness" | Existing issue (`djogi#150`); route through 4.0 audit; coordinate with 4E #148 (Amendment 3 still applies for the EXCLUDE-vs-WITHOUT-OVERLAPS preference clarification) |
| PG18 `array_sort()` / `array_reverse()` | `01-pg18-release-notes.md` lines 25–26 | No — scalar functions, raw SQL or CASE expressions cover today | Roadmapped; file `pg-18-scalars` tracking issue |
| PG18 `min`/`max` over arrays/composites | `01-pg18-release-notes.md` lines 32–35 | No — niche aggregates | Roadmapped; route to 4C tail after #89 type-state |
| `SAVEPOINT` family — **ergonomics on top of nested `atomic()` (which IS the typed surface)** — caller-named `savepoint(name: &str)` method | `03-pg18-sql-reference.md` lines 114–116 + `djogi/src/transaction.rs:13-18, 207-309` | No — anonymous-depth-named savepoints already work via nested `atomic()`; named API is convenience-only | New issue (ergonomics); post-v0.1.0; anchor: "nested `atomic()` IS the typed savepoint surface (`SAVEPOINT sp_<depth>` / `RELEASE` / `ROLLBACK TO`); a caller-named `savepoint(name)` shortcut is convenience" |
| FTS configuration DDL (CREATE TEXT SEARCH …) | `03-pg18-sql-reference.md` lines 244–256 | No — declarative, low-frequency | New issue, post-v0.1.0; raw bypass acceptable |
| `pgcrypto` **expression-side typed wrapper** (`encrypt`/`decrypt`/`digest`/`hmac`/`gen_salt` etc.) — extension itself IS reachable via the migration emitter allowlist (`djogi/src/migrate/bootstrap.rs:139`) | `04-extensions.md` line 35 + `djogi/src/migrate/bootstrap.rs:139` | No — adopters use Rust-side crypto today; extension is reachable, only the typed expression-surface is missing | New issue (expression-side wrapper, NOT extension absence); roadmapped post-v0.1.0 |
| `fuzzystrmatch` extension | `04-extensions.md` line 24 | No — pg_trgm covers main fuzzy-match shape | Out-of-scope or post-v0.1.0 with anchor |
| `ltree` extension | `04-extensions.md` line 31 | No — closure CTE covers most hierarchy queries | Out-of-scope for v0.1.0 |

**No item from this list is critical-blocking by itself.** MERGE is the
most probable alpha-blocker by adoption pressure (it's a PG15 feature
with broad use), and the OLD/NEW RETURNING is the most probable
audit-shaped feature gap. The rest are either Roadmapped or
Out-of-scope under the Area 19 dispositions.

## Quick-wins

Catalog says `unknown`, my grep proves `Implemented`. Audit ledger can
flip these without further verification.

1. **Postgres POSIX regex `~` / `~*`** (catalog implicit in
   `03-pg18-sql-reference.md`; not enumerated as a SQL-command-level
   row, but `unknown` for `regexp_like_named_args` and family in
   `01-pg18-release-notes.md` lines 43–48). **Evidence:**
   `djogi/src/query/filter.rs:114-118` and `djogi/src/query/predicate.rs:85`
   show `Q::Regex` is a typed query node; emitted via the
   `FieldRef::regex` / `iregex` surface. The PG18 named-arg variants
   (`regexp_match`, `regexp_replace`, etc.) are not exposed but the core
   POSIX `~` / `~*` operators are. **Disposition:** `Implemented` for
   `~` / `~*`; PG18 named-arg `regexp_*` functions are `Roadmapped`
   only if adopters need them.

2. **FTS `tsvector`/`tsquery`/`to_tsvector`/`to_tsquery`/`ts_rank`/
   `ts_rank_cd`** (catalog: implicit in `01-pg18-release-notes.md`
   line 29 Estonian FTS stemming, `04-extensions.md` line 56 unaccent,
   etc.; the underlying `to_tsvector` family isn't enumerated as a
   PG-feature row but its absence in the catalog `covered` set is
   misleading). **Evidence:** `djogi/src/expr/node.rs:387-438`
   implements `TsMatch`, `TsRank`, `TsRankCd`; `djogi/src/fts.rs`
   defines `Tsvector` and `Tsquery` types with `ToSql`/`FromSql`;
   `expr/sql.rs:1036-1105` emits the SQL. **Disposition:**
   `Implemented` for the basic FTS shapes.

3. **`select_for_update` + `nowait` + `skip_locked`** (catalog: not
   enumerated as a top-level row; `FOR UPDATE` is implicit in
   `03-pg18-sql-reference.md` line 11 SELECT). **Evidence:**
   `djogi/src/query/queryset.rs:1058-1082` and
   `djogi/src/query/lock.rs:69-122`. **Disposition:** `Implemented`.

4. **`ON CONFLICT (...) DO UPDATE` (UPSERT)** (catalog: implicit in
   `03-pg18-sql-reference.md` line 12 INSERT). **Evidence:**
   `djogi/src/query/closure.rs:594-622` and `djogi/src/transaction.rs`
   `retry_on_conflict` + `Model::save`'s upsert flag (typically
   surfaced via `Model::save` and ID pre-generation; descriptor work
   in #133 has touched this area). **Disposition:** `Implemented` for
   primary-key conflict; broader unique-index ON CONFLICT may be
   partial.

5. **GROUPING SETS** (catalog: implicit in
   `01-pg18-release-notes.md` aggregate features). **Evidence:**
   `djogi/src/query/queryset.rs:1417-1474` shows `GROUP BY GROUPING
   SETS ((col_a), (col_b), ...)` is implemented. The variadic
   `GROUPING(c1, ..., cN)` bitmask is what #94 still adds. **Disposition:**
   `Implemented` for GROUPING SETS shape; `Roadmapped` for the bitmask
   helper (#94).

6. **Window functions `ROW_NUMBER`, `DENSE_RANK`, `RANK`,
   `PERCENT_RANK`, `CUME_DIST`** plus PARTITION/ORDER BY composition
   (catalog: not separately enumerated). **Evidence:**
   `djogi/src/query/annotate.rs:865-933` shows RowNumber and
   DenseRank pin-tested; the qualify-lowering machinery is at
   `query/annotate.rs:566-711`. **Disposition:** `Implemented` for
   core window functions + qualify; `Roadmapped` for f64 fraction
   (#95).

7. **Spatial relationships `ST_Within` / `ST_Contains` / `ST_Intersects`
   / `ST_Disjoint` / `ST_Equals` / `ST_DWithin` / `ST_Distance` /
   `ST_Buffer` / `ST_Centroid` / `ST_ConvexHull` / `ST_Simplify`**
   (catalog: marked `covered` already). **Evidence:** all present in
   `djogi/src/expr/spatial.rs` (Within at line 326, Distance at 342,
   relationship operators at 481+). **Disposition:** confirms catalog
   `covered`.

8. **JSONB path operators `->`, `->>`, `(col->'a'->>'b')::cast`**
   (catalog: implicit in JSONB row of MASTER-CATALOG). **Evidence:**
   `djogi/src/jsonb/path.rs:261-290` (path emission) and
   `jsonb/path.rs:354-405` (eq/neq/gt/gte/lt/lte). **Disposition:**
   `Implemented`.

These eight quick-wins flip ~30+ catalog rows from `unknown` to
`Implemented` without any new grep — the audit ledger should record
them on first pass.

## Self-review caveat

This is a careful-coder cross-reference. It is NOT a GPT-5.5 xhigh
ALLOW. The user must dispatch independent review before treating this
as Cluster 4.0 deliverable.

**Re-eval status (`2026-05-10`, post-Opus correction pass):**
- SAVEPOINT framing is now correct (nested `atomic()` IS the typed
  surface; only an opaque caller-supplied savepoint name is missing).
- Temporal-constraint family is routed to the existing `djogi#150`
  (no new evaluation issue needed).
- pgcrypto framing is now correct (extension reachable via allowlist;
  expression-side typed wrapper is the gap).
- The quick-wins line refs (`djogi/src/expr/node.rs:398,417,431`,
  `djogi/src/fts.rs`, `djogi/src/query/queryset.rs:1081,1094,1108`,
  `djogi/src/query/lock.rs:69-122`, `djogi/src/query/queryset.rs:1417,1517`)
  are reviewer-confirmed. No quick-win removed; the bottleneck for
  flipping more `unknown` rows is a full catalog walk during the
  4.0 audit.

Specific things I did not verify and that an independent reviewer
should confirm:

- **MERGE coverage claim**: my grep was for `MERGE INTO` and
  `MergeStmt` and similar. If MERGE is implemented under a
  different name (e.g. as part of a save/upsert helper), my
  classification is wrong. **Re-eval reviewer pass confirmed
  zero `MERGE INTO` in `djogi/src` — claim stands.**
- **OLD/NEW RETURNING coverage**: zero matches for `RETURNING.*OLD`
  and `RETURNING.*NEW`, but the syntax may be supported through a
  different path (e.g. trigger-based audit hooks) that doesn't grep
  cleanly.
- **Quick-win row count (~30+)**: I sampled 8 quick-wins, generalized
  the count. The full audit may surface more or fewer.
- **Out-of-scope ~450-600 estimate**: order-of-magnitude rough; the
  audit will land closer to actuals.
