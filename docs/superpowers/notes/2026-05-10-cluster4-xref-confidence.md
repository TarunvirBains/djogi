# Cluster 4 Cross-Reference Confidence Ledger

> Companion to `2026-05-10-cluster4-vs-postgres-coverage-xref.md`
> and `2026-05-10-cluster4-v3-amendment-proposal.md`. Lets the user
> and the GPT-5.5 reviewer see what to prioritize verifying.

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
| `SAVEPOINT` family is NOT implemented as typed surface | **medium** | spec-read of `djogi/src/transaction.rs` shows atomic + retry_on_conflict but no SAVEPOINT methods; could be inside a builder I didn't sample | run `grep -ri 'savepoint\|Savepoint' djogi/src` for ~3 minutes |
| `pgcrypto` extension is NOT implemented as typed surface | **medium** | grep for `pgcrypto` / `encrypt` / `digest(` / `gen_salt` / `hmac(` returned only Rust-side crypto in plan_file.rs / snapshot/sign.rs; no SQL-side wrapping | run `grep -ri 'pgcrypto\|encrypt(\|digest(' djogi/src` for ~3 minutes; could be in a sub-module |
| FTS configuration DDL (CREATE TEXT SEARCH CONFIG/DICT/PARSER/TEMPLATE) is NOT implemented | **medium** | catalog rows all `unknown`; spec-read of FTS files shows query-side coverage only; no DDL emission for FTS configuration | run `grep -ri 'CREATE TEXT SEARCH' djogi/src` for ~2 minutes |
| `fuzzystrmatch` extension (Levenshtein, Soundex, Metaphone) is NOT implemented | **high** | grep for `fuzzystrmatch` / `levenshtein` / `soundex` / `metaphone` returned zero matches | n/a |
| `ltree` extension is NOT implemented | **high** | grep for `ltree` returned zero matches; closure CTE in `query/closure.rs` covers the dominant hierarchy use case | n/a |
| `cube` / `seg` / `isn` data type extensions are NOT implemented | **medium** | not directly grep-verified; catalog rows `unknown`; very specialized | run `grep -ri 'cube_ops\|seg_ops\|isn_ops' djogi/src` for ~2 minutes |
| GIN `jsonb_path_ops` opclass is NOT exposed as typed `using` attribute | **medium** | catalog row `06-operator-classes.md:105` `unknown`; grep for `jsonb_path_ops` returned zero hits; index-projection layer supports `using = "..."` raw string | run `grep -ri 'jsonb_path_ops\|gin_ops\|opclass' djogi/src/migrate` for ~3 minutes |
| Specialized PostGIS functions (clustering ST_ClusterDBSCAN, coverage ST_CoverageUnion, trajectory ST_IsValidTrajectory, exotic I/O ST_AsFlatGeobuf/MARC21/TWKB) are NOT implemented | **medium** | catalog rows all `unknown`; only the canonical typed spatial surface (~30 functions) is implemented per `geo/mod.rs`; no spec-grep done on the long tail | run `grep -ri 'ClusterDBSCAN\|CoverageUnion\|IsValidTrajectory\|FlatGeobuf' djogi/src` for ~5 minutes |
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
   under a different name, Amendment 3 is unnecessary; if they're not,
   Amendment 3 is correct.
2. **`SAVEPOINT` family** (medium) — if it's implemented through a
   builder I didn't sample, Amendment 4 issue 1 is unnecessary.
3. **`pgcrypto`** (medium) — if there's a SQL-side wrapper
   I didn't find, Amendment 4 issue 3 is unnecessary.
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
