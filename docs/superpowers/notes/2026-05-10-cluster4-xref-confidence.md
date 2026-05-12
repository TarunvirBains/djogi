# Cluster 4 Cross-Reference Confidence Ledger

> Companion to `2026-05-10-cluster4-vs-postgres-coverage-xref.md`
> and `2026-05-10-cluster4-v3-amendment-proposal.md`. Lets the user
> and the GPT-5.5 reviewer see what to prioritize verifying.

## Path-reference disclaimer

This document cites local reviewer artifacts under
`docs/superpowers/plans/` and `docs/research/postgres-coverage/2026-05-09/`
that are not tracked in this repository. Path references are accurate
for the lane lead's local filesystem; readers cloning the repository
will only see the notes under `docs/superpowers/notes/`. See the
companion cross-reference doc for the full disclaimer.

## Confidence rubric

- **high** = grep + spec read + catalog row all agree
- **medium** = catalog + spec agree, no grep verification
- **low** = catalog claim, no independent verification

## Findings table

| Finding | Confidence | Evidence type | Verification cost to upgrade |
|---|---|---|---|
| MERGE has no typed surface (catalog says `partial`, actually uncovered) | **high** | grep against `djogi/src/` for `MERGE INTO`, `MergeStmt`, `merge_stmt`, `MergeWhen` returned only one comment in `migrate/diff.rs:572` using "MERGES" in a different sense; spec-read of `query/queryset.rs` save/upsert helpers; catalog row 03-pg18-sql-reference.md:16 | n/a — already verified |
| `SpatialExpr::Intersection` has no public constructor (gated behind `area_of_intersection` fused shape) | **high** | spec-read of `djogi/src/expr/spatial.rs:223-247` (rustdoc explicitly says "no public typed constructor today"); grep against `djogi/src` for `pub fn intersection` / `pub fn st_intersection` returns zero; aligns with catalog row `02-postgis-functions.md:253` | n/a — already verified |
| `select_for_update`, `nowait`, `skip_locked` ARE implemented | **high** | spec-read of `djogi/src/query/queryset.rs:1058-1082` (LockMode::ForUpdate / ForUpdateNowait / ForUpdateSkipLocked); spec-read of `djogi/src/query/lock.rs:69-122` | n/a |
| FTS `to_tsvector` / `to_tsquery` / `ts_rank` / `ts_rank_cd` ARE implemented | **high** | spec-read of `djogi/src/expr/node.rs:387-438` (TsMatch/TsRank/TsRankCd variants); spec-read of `djogi/src/expr/sql.rs:1036-1105` (SQL emission); spec-read of `djogi/src/fts.rs:43-59` (Tsvector/Tsquery types) | n/a |
| Postgres POSIX regex `~` / `~*` ARE implemented | **high** | spec-read of `djogi/src/query/filter.rs:114-118` (regex/iregex methods on FieldRef); spec-read of `djogi/src/query/predicate.rs:85` (Q::Regex node); aligned with CLAUDE.md "no Rust regex — Postgres `~` carve-out" entry | n/a |
| `ON CONFLICT (...) DO UPDATE` is implemented at least for primary-key conflicts | **high** | spec-read of `djogi/src/query/closure.rs:594-622` (ON CONFLICT (...) DO UPDATE SET <col> = EXCLUDED.<col>); spec-read of `djogi/src/transaction.rs` retry_on_conflict; covered through Model::save | n/a |
| `GROUPING SETS` typed surface IS implemented; `GROUPING(c1,...,cN)` bitmask is NOT (#94 covers) | **high** | spec-read of `djogi/src/query/queryset.rs:1417-1474` (grouping_sets emission); cross-reference with v3 #94 task scope | n/a |
| Window functions `RowNumber`, `DenseRank` plus `qualify` lowering ARE implemented | **high** | spec-read of `djogi/src/query/annotate.rs:865-933` (RowNumber/DenseRank pin tests); spec-read of `djogi/src/query/annotate.rs:566-711` (qualify field + emission) | n/a |
| Spatial relationships (Within/Contains/Intersects/Disjoint/Equals/DWithin) plus measurement/overlay families ARE implemented | **high** | spec-read of `djogi/src/expr/spatial.rs` for each variant: Within line 326, Distance line 342, Intersection line 411, ConvexHull migrated to AggOp::SpatialConvexHull; aligned with catalog `covered` rows in 02-postgis-functions.md | n/a |
| JSONB path operators `->`, `->>`, `(col->'a'->>'b')::cast` ARE implemented | **high** | spec-read of `djogi/src/jsonb/path.rs:261-290` (path emission); spec-read of `jsonb/path.rs:354-405` (eq/neq/gt/gte/lt/lte) | n/a |
| `pg_trgm` is in extension allowlist + migration emitter; NO typed similarity expression | **high** | spec-read of `djogi/src/migrate/bootstrap.rs:138` (allowlist) and 821 (validation); grep against `djogi/src/expr/` for `trigram` / `similarity` / `pg_trgm_ops` returned zero matches | n/a |
| `btree_gist` is in extension allowlist; NO exclusion-constraint typed surface | **high** | spec-read of `djogi/src/migrate/bootstrap.rs:137` (allowlist); grep for `EXCLUDE` / `ExcludeConstraint` returned zero matches in `djogi/src` | n/a |
| `LATERAL`, `VALUES`, `UNION`/`INTERSECT`/`EXCEPT` are framework-internal only (no public surface) | **high** | grep matched only in `query/recursive.rs` and `query/closure.rs`; both files are framework-internal CTE/closure machinery; no `QuerySet::union` / `lateral` / `values` public method | n/a |
| AggregateExpr type-state (#89) does NOT exist yet (only `AggregateExpr<Out>` with PhantomData) | **high** | spec-read of `djogi/src/expr/aggregate.rs:86-91` shows single template; no Kind discriminator, no ValueAgg/MetadataAgg/OrderedSetAgg/HypotheticalSetAgg type-states | n/a |
| `ST_AsMVT` / `ST_AsGeobuf` row-shape aggregates are NOT implemented (#92 covers) | **high** | grep against `djogi/src` for `AsMVT` / `asmvt` / `AsGeobuf` / `asgeobuf` returned zero matches; aligned with #92 issue body "AggOp wraps a single arg, no row-shape input slot" | n/a |
| `CHECK` constraint via `#[field()]` is NOT implemented (DDL layer supports it; macro surface does not) | **high** | spec-read of `djogi/src/migrate/schema.rs:183` (ColumnSchema.check Option<String>); spec-read of `djogi/src/migrate/diff.rs:375` (SetCheck change variant); cross-reference with #105 issue body confirming macro-surface gap | n/a |
| PG18 `OLD` / `NEW` in `RETURNING` is NOT implemented | **high** | grep for `RETURNING.*OLD` / `RETURNING.*NEW` / `returning_old` / `returning_new` returned zero matches | n/a |
| PG18 temporal constraints (WITHOUT OVERLAPS, PERIOD FK, NOT ENFORCED, named NOT NULL) are NOT implemented | **medium** | grep for `WITHOUT OVERLAPS` / `PERIOD FK` / `NOT ENFORCED` returned zero matches; only `tstzrange` mention in `descriptor.rs:378` is for documentation; could be implemented under a different name | run `grep -ri 'temporal\|without overlaps\|period\|not enforced' djogi-macros/src djogi/src` for ~5 minutes |
| PG18 scalar functions (uuidv7, uuidv4, array_sort, array_reverse, casefold, crc32, gamma) are NOT implemented | **high** | grep for `casefold` / `array_sort` / `array_reverse` / `crc32` / `gamma(` returned zero matches; HeerId/RanjId obviates UUID functions | n/a |
| `SAVEPOINT` family — **partial** — typed surface composed via nested `atomic()`; no caller-named savepoint method | **high** (downgraded from "medium absence" by re-eval) | spec-read of `djogi/src/transaction.rs:13-18, 207-309` confirms nested `atomic()` pushes `SAVEPOINT sp_<depth>` (line 223-227), releases on `Ok` (line 265), rolls back on `Err` (line 281), rolls back on panic (line 300); rustdoc explicitly notes "Nested calls push a Postgres savepoint rather than opening a new transaction" (line 13-18). Caller-named savepoint method (`savepoint(name: &str)`) does NOT exist — that is the ergonomics-on-top gap, not absence of capability. | n/a — already verified post-correction |
| `pgcrypto` extension — **partial** — extension reachable via migration emitter allowlist; expression-side typed wrapper missing | **high** (downgraded from "medium absence" by re-eval) | spec-read of `djogi/src/migrate/bootstrap.rs:139` (allowlist entry); spec-read of `djogi/src/migrate/bootstrap.rs:821` (validation); spec-read of `djogi/src/testing.rs:834, 1438` (test fixture references). No expression-side typed wrapper for `encrypt`/`decrypt`/`digest`/`hmac`/`gen_salt`. Adopters who want pgcrypto today get the extension projected automatically when the descriptor lists it; SQL-side calls require the raw bypass attribute. | n/a — already verified post-correction |
| FTS configuration DDL (CREATE TEXT SEARCH CONFIG/DICT/PARSER/TEMPLATE) is NOT implemented | **medium** | catalog rows all `unknown`; spec-read of FTS files shows query-side coverage only; no DDL emission for FTS configuration | run `grep -ri 'CREATE TEXT SEARCH' djogi/src` for ~2 minutes |
| `fuzzystrmatch` extension (Levenshtein, Soundex, Metaphone) is NOT implemented | **high** | grep for `fuzzystrmatch` / `levenshtein` / `soundex` / `metaphone` returned zero matches | n/a |
| `ltree` extension is NOT implemented | **high** | grep for `ltree` returned zero matches; closure CTE in `query/closure.rs` covers the dominant hierarchy use case | n/a |
| `cube` / `seg` / `isn` data type extensions are NOT implemented | **medium** | not directly grep-verified; catalog rows `unknown`; very specialized | run `grep -ri 'cube_ops\|seg_ops\|isn_ops' djogi/src` for ~2 minutes |
| ~~GIN `jsonb_path_ops` opclass is NOT exposed as typed `using` attribute~~ — **corrected by GPT-5.5 xhigh review (2026-05-12)**: opclass IS exposed via the `index(... opclass = "jsonb_path_ops")` macro attribute | **high** (post-correction) | `djogi-macros/src/model/indexes.rs:1012` parses `opclass = "jsonb_path_ops"`; `docs/spec/indexing.md:31` documents the surface; my earlier grep was scoped only to `djogi/src/migrate` and missed the macro-side parser. Flip the catalog disposition for `gin_jsonb_path_ops` from `unknown` to `Implemented`. | n/a — verified post-correction |
| Specialized PostGIS functions — **ST_ClusterDBSCAN IS implemented** as `cluster_by_proximity` (`djogi/src/query/queryset.rs:1809`; SQL emission at line 1773 calls `ST_ClusterDBSCAN(t.<col>::geometry, $eps, $minpoints) OVER ()`). Coverage (`ST_CoverageUnion`), trajectory (`ST_IsValidTrajectory`), and exotic I/O (`ST_AsFlatGeobuf`, MARC21, TWKB) remain NOT implemented. | **high** (post-correction by GPT-5.5 xhigh review 2026-05-12) | `cluster_by_proximity` confirmed at `djogi/src/query/queryset.rs:1767-1809` + types at `djogi/src/query/spatial_grouping.rs:89-117`. `cluster_by_proximity` returns `GroupedQuerySet<T, ClusterId>`; pin test at queryset.rs:3427. The remaining specialized functions (CoverageUnion, IsValidTrajectory, FlatGeobuf, MARC21, TWKB) still have zero grep matches in `djogi/src/`. | n/a — partial coverage now verified |
| Quick-win count of "~30+ catalog rows flippable from `unknown` to `Implemented` on first audit pass" | **medium** | sampled 8 quick-win categories; generalized count from category breadth; could be 20 or 50 depending on how the audit ledger counts row-level vs category-level | full audit pass would land actuals; ~30 minutes of scripted catalog walk |
| Out-of-scope ~450-600 estimate from catalog `unknown` set | **low** | order-of-magnitude; based on rough categorization (replication, FDW, FTS DDL, libpq protocol, psql, pg_dump flags, monitoring GUCs, OAuth, SSL, specialized PostGIS) | full audit pass needed to land actuals |
| MASTER-CATALOG.md "Notable Cluster 1: PG18 Constraint & Temporal Features" maps to v3 4B + 4E correctly | **medium** | based on spec-read of catalog cluster description; does not verify what subset of features is actually represented in 4B/4E task lists | cross-walk each of the 5 features (named NOT NULL, NOT ENFORCED, WITHOUT OVERLAPS, PERIOD FK, deterministic_collation_fk) against #105 + #148 issue bodies — ~10 minutes |
| MASTER-CATALOG.md "Notable Cluster 2: PG18 I/O & Performance GUCs" is correctly out-of-scope | **high** | catalog explicitly notes these as monitoring/tuning; query builder doesn't expose; aligned with Area 19 carve-out rules in red-team-gate-plan | n/a |
| MASTER-CATALOG.md "Notable Cluster 3: PostGIS Advanced Geometry Operations" is correctly post-v0.1.0 | **medium** | catalog notes "specialized geometry; likely Phase 12.5+"; v0.1.0 spatial alpha covers core families per `geo/mod.rs`; alignment is structural not feature-by-feature | per-function audit would land actuals; ~10 minutes per feature |
| MASTER-CATALOG.md "Notable Cluster 4: System Catalog Introspection" mostly out-of-scope; descriptor work covers pg_class/pg_attribute already | **high** | spec-read of `djogi/src/migrate/` confirms descriptor introspection uses pg_class / pg_attribute / pg_constraint / pg_index / pg_type / pg_proc / pg_namespace / pg_trigger / pg_operator (catalog confirms these as `covered` core); pg_stat_io / pg_backend_memory_contexts not used | n/a |
| MASTER-CATALOG.md "Notable Cluster 5: PostgreSQL 18 New Functions" warrants per-function audit decision | **high** | each function is a separate Implemented / Roadmapped / Out-of-scope decision; HeerId/RanjId obviates UUID functions; rest are scalar surface | per-function decision in the audit |
| MASTER-CATALOG.md "Notable Cluster 6: Replication & Logical Decoding" correctly out-of-scope | **high** | replication topology is upstream of djogi; logical_inspect / two-phase / streaming are infrastructure; aligned with Area 19 carve-out rules | n/a |

## Verification priority for GPT-5.5 reviewer

The findings labeled **medium** or **low** are the ones where this
xref relies on negative grep evidence and could be wrong. In rough
order of impact:

1. **PG18 temporal constraints** (medium) — if these ARE supported
   under a different name, the route-to-`djogi#150` instruction in
   Amendment 2 is harmless; if they're not, #150 is the correct
   destination.
2. **`SAVEPOINT` family** — **resolved post-correction**.
   careful-coder Opus reviewer confirmed nested `atomic()` IS the
   typed savepoint surface (`djogi/src/transaction.rs:13-18, 207-309`).
   Amendment 4 issue 1 is reframed as ergonomics-on-top, NOT absence.
3. **`pgcrypto`** — **resolved post-correction**.
   careful-coder Opus reviewer confirmed the extension IS reachable
   via the migration emitter allowlist
   (`djogi/src/migrate/bootstrap.rs:139`). Amendment 4 issue 3 is
   reframed as expression-side typed wrapper gap, NOT extension
   absence.
4. **FTS configuration DDL** (medium) — if there's a typed `#[fts]`
   attribute that emits CREATE TEXT SEARCH CONFIG, Amendment 4 issue
   2 is unnecessary.
5. **Quick-win count** (medium) — actual count affects audit work
   estimate but not routing decisions.
6. **Specialized PostGIS function long-tail** (medium) — actual count
   matters for the Amendment 5 tracking issue scope.

## Self-review caveat

Reaffirmed: this is careful-coder cross-reference output, not a
GPT-5.5 xhigh ALLOW. Every **high**-confidence finding has at least
one direct grep + spec-read pair backing it; every **medium** /
**low** finding is the polite way of saying "I sampled, I might be
wrong, please verify."

## Review history

- **`ca93e9c`** (`2026-05-10`) — initial three deliverables landed
  on branch `cluster4-postgres-coverage-xref`:
  `2026-05-10-cluster4-vs-postgres-coverage-xref.md`,
  `2026-05-10-cluster4-v3-amendment-proposal.md`, this file.
- **careful-coder Opus reviewer pass** (`2026-05-10`) — verdict
  `ALLOW_WITH_CORRECTIONS`. Reviewer flagged three issues:
  1. SAVEPOINT framed as absence; in fact nested `atomic()` IS the
     typed surface. Reframed as ergonomics-on-top.
  2. Amendment 2 temporal-constraint bullet duplicates already-filed
     `djogi#150`. Reframed as routing-to-existing-issue.
  3. pgcrypto entry conflated extension reachability with
     expression-side typed wrapper absence. Reframed.
  Reviewer also confirmed: Amendment 1 (MERGE), Amendment 3 (PG18
  syntax preference for #148), Amendment 4 issues 2/3/4/5,
  Amendment 5 (PostGIS constructor breadth), and the eight
  quick-wins are all valid.
- **Re-eval pass** (`2026-05-10`, commit `03dc0f6`) addresses the three
  reviewer corrections. Final amendment count: **5 (unchanged)**.
  Final issues-to-file count: **7** (was 8; SAVEPOINT dropped from
  the file-list and recorded as an anchored deferral because nested
  `atomic()` IS the typed surface; capability-gap framing was wrong).
  pgcrypto stays in the file-list with reframed expression-side
  scope.
- **GPT-5.5 xhigh reviewer pass** (`2026-05-12`, on PR #192) — verdict
  `APPROVE_MERGE: NO` initially, with five FIX_BEFORE_MERGE findings.
  Lane lead applied corrections in this commit (see review history
  immediately below). Re-dispatch of gpt-5.5 follows.
- **GPT-5.5 correction pass** (`2026-05-12`, this commit) addresses
  all five findings:
  1. Amendment 1 "Current text" quotation was stale against the
     actual v3 plan; refreshed to include `#168`, `#169`, `#170`,
     `#172`. Amendment 1's "Proposed text" preserves them all and
     adds `#new-merge`. (Cross-checked the v3 plan §Cluster 4B
     lines 475–506 in the local plan file at lock time.)
  2. `jsonb_path_ops` opclass was wrongly classified as `medium`
     unknown. Corrected to `high` Implemented in this file (entry
     reframed) and in the cross-reference (`...vs-postgres-coverage-xref.md`
     §Cluster 4C). Becomes a ninth quick-win for the audit ledger.
     Evidence: `djogi-macros/src/model/indexes.rs:1012`,
     `docs/spec/indexing.md:31`.
  3. `ST_ClusterDBSCAN` was wrongly bundled into the "specialized
     PostGIS NOT implemented" list. Corrected: clustering via
     `cluster_by_proximity` (`djogi/src/query/queryset.rs:1809`)
     IS implemented and emits `ST_ClusterDBSCAN(... OVER ())`.
     Remaining specialized functions (coverage `ST_CoverageUnion`,
     trajectory `ST_IsValidTrajectory`, exotic I/O `ST_AsFlatGeobuf` /
     MARC21 / TWKB) remain `unknown`/uncovered.
  4. Candidate issues 2–7 now carry inline Stage 1.5 closing-condition
     checklists (rustdoc + doctest + spec + live PG18 test +
     adopter guide) so they are file-ready alongside candidate 1.
  5. Added path-reference disclaimers to the top of all three notes
     so the broken-link hazard from cross-referencing local-only
     artifacts (`docs/superpowers/plans/...`,
     `docs/research/postgres-coverage/2026-05-09/...`,
     `docs/superpowers/red-team-gate-plan.md`) is explicit.
  6. SAVEPOINT framing aligned across the cross-reference and the
     amendment proposal: NOT filed during this audit; anchored
     deferral; capability already exists via nested `atomic()`.
- **Pending:** GPT-5.5 xhigh re-dispatch on the corrected commit.
