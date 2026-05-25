# Issue #126 Robust Implementation Plan

## Executive Recommendation

Implement #126 by making generated `{Model}Filter: IntoQ<Model>` preserve Q algebra through a lazy, single-source conversion from its existing `Vec<FilterClause>` into `Q<Model>` at consumption time.

Chosen defaults:

- Keep `FilterClause` / `clauses: Vec<FilterClause>` as the sole stored source of truth in generated filters.
- Do not add persistent `q_leaves: Vec<Q<Model>>` or any parallel cache of Q leaves.
- Convert clauses into Q only inside generated `IntoQ<Model>` for `{Model}Filter`.
- Preserve the existing public `filter_struct<F: IntoQ<T>>()` API; do not add a new public `.filter_q()` unless the owner explicitly prefers a more explicit alias.
- Fold empty filters as `Q::always_true()`, one clause as one Q leaf, and multiple clauses as an `And` Q tree.
- Preserve all-or-nothing cache/refresh reduction. Mixed trees may retain partial Q structure, but `refresh_into` and cache admission must still reject the whole query if any nonportable residual node remains.
- Gracefully fallback to `Q::Condition(FilterClause::into_condition())` for unsupported ops, unsupported field kinds, value-shape mismatches, and future `FilterValue` variants.
- Keep JSONB path and proxy default filters out of this fix unless the owner explicitly expands the scope.
- Treat string pattern `Lookup::{Contains, StartsWith, EndsWith}` conservatively: fallback to `Q::Condition` by default until the owner accepts the portable ASCII-stable `COLLATE "C" ILIKE ... ESCAPE` semantics and fixture/docs churn.

This closes the main correctness gap from #126 without introducing a database/cache divergence channel. The skeletons' dual-state design is mechanically possible because `Q<T>` is manually `Clone`, but it creates two representations that can disagree. Under the owner lens, that is the wrong default for production safety.

## Validated Facts

Issue state:

- GitHub issue #126 is open and its body is stale relative to the current repo. It says `.filter()/.filter_struct()/.exclude()` always lower through `and_condition_into_q`; current closure filters already compose direct Q.
- The owner comment on #126 requires docs before closure: rustdoc with a doctest-shaped Q-algebra example through `.filter()`, prelude docs if a new shape surfaces there, a doc comment explaining use case versus closure `.filter(|f| ...)`, an `examples/elephant-tracker/` update alongside #99 if in the same cluster, and Phase 8 spec amendments.

Current query composition:

- `djogi/src/query/queryset.rs:640` defines `and_q_into_q`; it directly composes `Q<T>` and only short-circuits vacuous true inputs.
- `djogi/src/query/queryset.rs:717` seeds proxy default filters with `T::default_filter_condition().map_or_else(Q::always_true, Q::Condition)`, so proxy defaults remain a `Q::Condition` source.
- `djogi/src/query/queryset.rs:779` makes `filter` accept `P: IntoQ<T>` and compose with `and_q_into_q`.
- `djogi/src/query/queryset.rs:893` makes `filter_struct` accept any `IntoQ<T>`, but its docs still describe legacy `Condition` behavior for `{Model}Filter`.
- `djogi/src/query/queryset.rs:951` implements `exclude_struct` by negating the `IntoQ` result.

Current reducer/cache behavior:

- `djogi/src/query/queryset.rs:2334` reduces `Q::{Portable, Compound, Xor, Negated}` but returns `CacheInvalidNode { kind: "Condition" }` for `Q::Condition`.
- `djogi/src/query/queryset.rs:2403` makes `try_portable` return `Err((self, err))` without starting terminal cache work on reduction/validation failure.
- `djogi/src/query/queryset.rs:2447` documents `into_basic_predicate` as all-or-nothing: if any node cannot reduce, it returns `None`.
- `djogi/src/query/queryset.rs:2562` makes `refresh_into` gate through `try_portable` and return the queryset plus error on nonportable filters.
- `djogi/src/query/queryset.rs:3499` contains reducer unit tests, including explicit rejection for legacy `Condition` and `Negated(Condition)`.

Current Q model:

- `djogi/src/query/q.rs:165` defines `Q::Portable`.
- `djogi/src/query/q.rs:209` defines `Q::Condition` as the legacy escape hatch for SQL-only filters.
- `djogi/src/query/q.rs:244` documents empty `Q::Compound` semantics as `And => true` and `Or => false`.
- `djogi/src/query/q.rs:280` manually implements `Clone` and `Debug` for `Q<T>`, avoiding a `T: Clone` or `T: Debug` requirement.
- `djogi/src/query/q.rs:491` documents that generated `{Model}Filter` still folds clauses through `clauses_into_condition` and wraps `Q::Condition`.
- `djogi/src/query/q.rs:603` implements `IntoQ<T> for Q<T>`.
- `djogi/src/query/q.rs:816` and `djogi/src/query/q.rs:995` lower Q trees to legacy `Condition` for SQL emission compatibility.

Current SQL emission:

- `djogi/src/query/sql.rs:946` emits `Q` directly for SQL.
- `djogi/src/query/sql.rs:1013` emits empty `Q::Compound(And)` as `TRUE` and empty `Q::Compound(Or)` as `FALSE`.
- `djogi/src/query/sql.rs:375` emits legacy `Condition` string pattern lookups with plain `ILIKE`.
- `djogi/src/query/portable.rs:1664` emits portable string pattern predicates with `COLLATE "C" ILIKE ... ESCAPE '\\'`. This is not necessarily identical to the legacy `Condition` path, especially for wildcard escaping and non-ASCII behavior.

Current programmatic filter model:

- `djogi/src/query/filter.rs:1` documents `FilterClause` / `ModelFilter` as the closure-free path for shell/admin/dynamic callers.
- `djogi/src/query/filter.rs:85` defines `Lookup`.
- `djogi/src/query/filter.rs:127` maps `Lookup` into `(LookupOp, FilterValue)`.
- `djogi/src/query/filter.rs:195` defines `FilterClause` with crate-private `column`, `op`, and `value`.
- `djogi/src/query/filter.rs:255` converts a `FilterClause` into legacy `Condition`.
- `djogi/src/query/filter.rs:265` defines `ModelFilter` as `fn into_clauses(self) -> Vec<FilterClause>`.
- `djogi/src/query/filter.rs:302` implements `clauses_into_condition`: empty means `Condition::True`, one clause unwraps to a leaf, many clauses become a flat `Condition::And`.

Generated filter state:

- `djogi-macros/src/model/filter.rs:166` generates `{Model}Filter` without access to `portable_field_info`.
- `djogi-macros/src/model/filter.rs:251` makes setters push only `FilterClause::from_lookup(...)`.
- `djogi-macros/src/model/filter.rs:288` generates `IntoQ` by calling `clauses_into_condition` and wrapping `Q::Condition`.
- `djogi-macros/src/model/filter.rs:304` stores only `clauses: Vec<FilterClause>`.
- `djogi-macros/src/model/filter.rs:326` implements `ModelFilter::into_clauses`.

Existing field-kind metadata:

- `djogi-macros/src/model/mod.rs:404` builds `portable_field_info` and already shares it with `{Model}Fields` and CRUD portable predicate SQL arms.
- `djogi-macros/src/model/stubs.rs:68` generates typed `{Model}Fields` accessors from `portable_field_info`.
- `djogi-macros/src/model/portable_field_emit.rs:61` classifies fields into portable scalar/string/bool/option/array/relation and SQL-only JSONB/spatial/FTS/unsupported kinds.
- `djogi-macros/src/model/portable_field_emit.rs:161` provides helper methods including `is_portable_leaf`, `is_optional`, `supports_string_patterns`, and `supports_ordering`.
- `djogi-macros/src/model/crud.rs:2968` emits the model-level portable predicate SQL dispatcher from the same field metadata.

Existing field/predicate APIs:

- `djogi/src/query/field.rs:611` documents the typed `DjogiField` method surface and which operations are portable versus legacy SQL-only.
- `djogi/src/query/field.rs:1252` provides portable equality and list methods.
- `djogi/src/query/field.rs:1398` provides portable ordering methods.
- `djogi/src/query/field.rs:1464` provides portable null tests for optional fields.
- `djogi/src/query/field.rs:1510` provides portable string pattern methods.
- `djogi/src/query/field.rs:2820` keeps legacy `FieldRef` comparison methods returning `Condition`.
- `djogi/src/query/field.rs:2982` keeps legacy SQL-only `FieldRef` string methods returning `Condition`.
- `djogi/src/query/predicate.rs:73` makes `PortablePredicate::from_djogi_field` `pub(crate)` and gated by `DjogiFieldProvenance`; macro-generated external code cannot directly mint arbitrary portable predicates.

Existing tests that must change or grow:

- `tests/integration/phase8_t8_4_basic_predicate_extraction.rs:94` proves closure portable filters reduce.
- `tests/integration/phase8_t8_4_basic_predicate_extraction.rs:136` currently asserts `filter_struct(PostFilter::new().active(true))` returns `None`; this should invert for portable fields.
- `tests/integration/phase2_queryset.rs:302` asserts row-set parity between closure `.filter(...)` and `filter_struct(PostFilter...)`.
- `tests/integration/phase2_queryset.rs:345` asserts empty `filter_struct` is identity.
- `tests/integration/phase2_queryset.rs:361` asserts single-clause filter struct behavior.
- `tests/integration/phase8eta_pr4_cache_refresh_gate.rs:222` proves `refresh_into` pushes closure portable filters into SQL.
- `tests/integration/phase8_5_c3_110_dogfood_round2.rs:456` proves prebuilt `Q<T>` can already be passed through `filter_struct`.

Docs/examples that must be updated:

- `docs/guide/queries.md:280` documents programmatic filters and `filter_struct`.
- `docs/guide/queries.md:807` documents cache/refresh portability gates.
- `docs/roadmap/querying.md:235` documents programmatic filter ergonomics.
- `docs/spec/implementation-plan.md:657` documents Q algebra and cache reducer behavior.
- `docs/superpowers/plans/2026-05-09-phase8-5-alpha-readiness-v3.md:353` already identifies #126 as needing Q-structure preservation for generated filters and explicit fixture policy.
- `examples/elephant-tracker/src/demos/mating_pairs.rs:286` contains a cache showcase that can host a small `ModelFilter` or Q-algebra refresh example if #99 touches the same area.

JSONB/#161 context:

- `docs/guide/jsonb.md:100` documents JSONB path references and comparisons.
- `djogi/src/jsonb/path.rs:356` implements JSONB path comparison methods returning `Condition`, not `PortablePredicate`.
- `djogi/src/query/queryset.rs:2357` rejects `Q::JsonbPath` at cache/refresh reduction time.
- #161 concerns custom `primary_key!` newtype JSONB path cast dispatch. That is orthogonal to #126 unless docs/tests around JSONB portability are edited in the same patch.

## Owner Decision Packet

Hard blockers:

- Partial pushdown with SQL-only residuals: if the owner wants cache/refresh to push portable subtrees while applying nonportable residuals elsewhere, the plan must change. The current reducer deliberately rejects mixed trees all-or-nothing, which avoids returning cache results that only satisfy part of the database predicate.
- Proxy default filter Q migration: if the owner wants proxy seeds to become Q-portable in #126, this expands into a `Model` trait/API design around `default_filter_condition`. Default is to defer proxy defaults because they are a separate source of `Q::Condition`.
- Character-for-character SQL parity: if exact SQL text parity is mandatory for all generated `ModelFilter` paths, portable mapping may be blocked for some operations. Default is semantic parity with explicit fixture updates where portable emission has intentional SQL shape differences.

Planner-default decisions:

- Single-source lazy mapping: proceed with `clauses -> Q` conversion at `IntoQ` time.
- No `q_leaves`: do not store Q leaves next to clauses.
- No new public filter method: keep `filter_struct` as the public entry point for `ModelFilter` and prebuilt `Q`.
- Conservative string patterns: fallback `Contains`, `StartsWith`, and `EndsWith` to `Q::Condition` until owner accepts the portable string semantics.
- All-or-nothing reducer: preserve current cache/refresh rejection behavior for any nonportable node.
- JSONB remains SQL-only: do not attempt JSONB path portability as part of #126.
- Proxy default filters remain legacy `Condition`: document residual behavior and defer.

Recommended owner review questions before implementation:

1. Accept fallback-to-`Condition` for string pattern `ModelFilter` clauses in this issue, or intentionally migrate them to portable ASCII-stable semantics and update fixtures?
2. Confirm proxy default filters are out of #126 scope.
3. Confirm no new `.filter_q()` public API is desired.
4. Confirm no partial cache pushdown/residual evaluation design is desired in this issue.

## Implementation Plan

### 1. Preserve the Source-of-Truth Invariant

Owner/module: `djogi/src/query/filter.rs`, `djogi-macros/src/model/filter.rs`.

Keep generated filters shaped as:

- `pub struct {Model}Filter { clauses: Vec<FilterClause> }`
- setters push exactly one `FilterClause`
- `ModelFilter::into_clauses(self)` consumes that vector

Do not add:

- `q_leaves`
- parallel vectors
- cached conditions
- cached portable predicates

Rollback boundary: if the new conversion proves problematic, reverting generated `IntoQ` back to `clauses_into_condition` restores the current behavior without changing setter storage or public filter state.

### 2. Add a Clause-to-Q Fold Helper

Owner/module: `djogi/src/query/filter.rs`.

Add a crate/private helper exported through `__private::query` only as needed by generated macro code:

- likely name: `clauses_into_q_with<T, F>(clauses: Vec<FilterClause>, map: F) -> Q<T>`
- `F: FnMut(FilterClause) -> Q<T>`
- `[] -> Q::always_true()`
- `[one] -> map(one)`
- `many -> Q::Compound(And, leaves)` or equivalent `&` fold that preserves explicit `And`

The helper must consume clauses and avoid cloning `FilterValue` where possible. It must not panic on empty input.

Also add a controlled way for generated conversion code to inspect a clause:

- either crate-private `FilterClause::into_parts(self) -> (&'static str, LookupOp, FilterValue)`
- or crate-private accessors used only inside a hidden helper

Keep `clauses_into_condition` unchanged for legacy paths and tests.

Rollback boundary: the new helper is additive and can be unused if generated `IntoQ` is reverted.

### 3. Share Portable Field Metadata with Filter Generation

Owner/module: `djogi-macros/src/model/mod.rs`, `djogi-macros/src/model/filter.rs`.

Change `filter::expand` to accept `portable_field_info: &[PortableFieldEmitInfo]`.

Use the same `portable_field_info` vector already used by:

- `djogi-macros/src/model/stubs.rs`
- `djogi-macros/src/model/crud.rs`

Reason: generated filter-to-Q mapping must not reimplement field classification. Drift here would be a correctness bug.

Rollback boundary: this is a macro-internal signature change. Reverting the generated `IntoQ` body and signature call restores the old generated filter behavior.

### 4. Generate a Per-Model Clause Mapper

Owner/module: `djogi-macros/src/model/filter.rs`.

Replace the generated `IntoQ` body that currently does:

- `into_clauses`
- `clauses_into_condition`
- `Q::Condition`

with:

- `let clauses = <Self as ::djogi::ModelFilter>::into_clauses(self);`
- call the new fold helper
- map each `FilterClause` through a generated `match` on `(column, op, value shape)`

Mapping policy:

- Equality/list ops: map to portable Q only for field kinds that support equality and value variants that match the generated field type.
- Ordering ops: map only when `PortableFieldKind::supports_ordering()` and value variants match.
- Null ops: map only for optional/nullable field kinds.
- Regex/IRegex: fallback to SQL-only Q (`Q::Regex` if the existing Q variant can preserve structure safely for string fields) or `Q::Condition`.
- String pattern ops: default fallback to `Q::Condition` unless owner approves portable string pattern semantics.
- JSONB/spatial/FTS/unsupported fields: fallback to `Q::Condition`.
- Unknown column/op/value variant: fallback to `Q::Condition`.

Do not expose public raw predicate constructors. If generated code cannot use public `DjogiField` methods for all required portable mappings because provenance constructors are crate-private, add a hidden djogi-owned helper in `djogi/src/query` and route it through `__private`. That helper must mint provenance inside the `djogi` crate and must validate against the same model field dispatcher; it must not become a public arbitrary `sassi::BasicPredicate` ingress.

Recommended implementation direction:

- First try to map by calling public typed `{Model}Fields` methods where the generated field type makes that possible.
- If public method coverage is awkward or reveals type gaps, add a hidden helper rather than widening public constructors.
- Keep all fallback behavior inside generated code or the hidden helper; never panic on a mismatched `FilterValue`.

Rollback boundary: generated `IntoQ` is the primary behavior switch. Restoring the old body returns all generated filters to `Q::Condition`.

### 5. Preserve Ordinary SQL Query Behavior

Owner/module: `djogi/src/query/sql.rs`, integration tests.

Do not change `emit_q` unless tests reveal a mismatch from the new generated Q shape.

Expected result:

- ordinary database `.filter_struct(...)` queries still emit SQL through `emit_q`
- portable leaves emit through portable predicate SQL
- fallback leaves emit through legacy `Condition`
- row-set results remain equivalent to closure filters and legacy filters

If string pattern lookups remain fallback-to-`Condition`, their SQL shape remains legacy.

If owner chooses portable string patterns, update SQL-shape fixtures deliberately and document the `COLLATE "C" ... ESCAPE` semantic choice.

Rollback boundary: if SQL row-set parity fails, narrow the mapped op set and fallback more clauses to `Q::Condition`.

### 6. Maintain All-or-Nothing Cache/Refresh Semantics

Owner/module: `djogi/src/query/queryset.rs`.

Do not make the reducer partially successful. The current semantics are correct for safety:

- `Q::Portable` reduces
- `Q::Compound` reduces only when every child reduces
- `Q::Negated` reduces only when its child reduces
- `Q::Condition`, `Q::JsonbPath`, `Q::Expression`, `Q::Regex`, `Q::Ilike`, and arrays remain nonportable unless separately proven

After #126:

- `filter_struct(PostFilter::new().active(true))` should reduce and refresh.
- `filter_struct(PostFilter::new().active(true).title_contains("x"))` should reject cache/refresh if `title_contains` remains a fallback condition.
- `into_basic_predicate()` should return `None` for mixed portable/nonportable generated filters.
- `refresh_into()` should return `Err((queryset, PortablePredicateError::CacheInvalidNode { ... }))` without starting terminal work for mixed filters.

Rollback boundary: reducer behavior should remain unchanged. Any required change here is a signal the generated mapper is too broad.

### 7. Update Public Docs and Examples

Owner/modules:

- `djogi/src/query/queryset.rs`
- `djogi/src/query/filter.rs`
- `djogi/src/query/q.rs`
- `docs/guide/queries.md`
- `docs/roadmap/querying.md`
- `docs/spec/implementation-plan.md`
- `examples/elephant-tracker/`

Required doc updates:

- Update `filter_struct` docs to say generated `ModelFilter` now preserves Q structure for portable clauses and falls back for SQL-only clauses.
- Add rustdoc/doctest-shaped example demonstrating Q-algebra composition through `.filter()` or `.filter_struct()` with generated filters.
- Explain `ModelFilter` use case versus closure `.filter(|f| ...)`: closure filters are preferred for statically typed Rust code; `ModelFilter` is useful for dynamic/admin/shell/form-derived filters.
- Document cache/refresh behavior: all-portable generated filters can reduce; mixed filters are rejected as a whole.
- Add a Phase 8 spec amendment describing the clause-to-Q mapping and all-or-nothing reducer contract.
- Update elephant-tracker only if it is in the same landing cluster as #99 or already being edited; otherwise add a small follow-up docs issue/commit note. If updated, the mating-pairs cache showcase is the likely location.

Rollback boundary: docs can be reverted independently, but issue #126 should not close without them.

## Tests Plan

Do not run tests during planning. These are the focused tests for the implementer.

Primary integration tests:

- `tests/integration/phase8_t8_4_basic_predicate_extraction.rs`
  - Invert `filter_struct_with_model_filter_returns_none`.
  - Add `filter_struct_with_portable_model_filter_extracts_basic_predicate`.
  - Add `filter_struct_with_empty_model_filter_is_vacuously_true_or_none_consistent`.
  - Add mixed portable + fallback generated filter case returning `None`.

- `tests/integration/phase8eta_pr4_cache_refresh_gate.rs`
  - Add `refresh_full_tick_pushes_model_filter_portable_filter`.
  - Add `refresh_rejects_model_filter_mixed_portable_and_condition`.
  - Assert no terminal handle starts for rejected mixed filters.

- `tests/integration/phase2_queryset.rs`
  - Keep existing row-set parity tests passing.
  - Add parity coverage for any newly portable mapped ops beyond current `active(true)` / `views_gte(...)`.
  - Preserve empty filter identity.
  - Add or preserve `exclude_struct(ModelFilter::new())` behavior, because `NOT TRUE` should still mean false.

Macro tests:

- `djogi-macros/tests/compile_pass/`
  - Add a compile-pass fixture proving generated `ModelFilter` still builds with portable scalar, option, and fallback SQL-only fields.
  - Add a fixture proving a prebuilt `Q<Model>` and `{Model}Filter` can compose through `filter_struct` without type inference regressions.

- `djogi-macros/tests/compile_fail/`
  - Add only if a new hidden helper has user-visible misuse modes. Prefer not exposing such a helper publicly.

Unit tests:

- `djogi/src/query/filter.rs`
  - Add tests for `clauses_into_q_with` empty, one, many, and fallback mapping.
  - Keep `clauses_into_condition` tests unchanged.

- `djogi/src/query/queryset.rs`
  - Add reducer unit coverage only if new Q shapes are introduced. Existing reducer tests should not need broad edits.

Docs/examples tests:

- Add doctest-shaped examples in rustdoc, but keep them realistic with the repo's macro setup.
- If examples are compiled in CI, update `examples/elephant-tracker/` README or demo code with a small ModelFilter/Q-algebra cache example.

Future focused verification commands:

- `cargo test -p djogi --test phase8_t8_4_basic_predicate_extraction`
- `cargo test -p djogi --test phase8eta_pr4_cache_refresh_gate`
- `cargo test -p djogi --test phase2_queryset`
- `cargo test -p djogi query::filter`
- macro fixture runner from this repo only; do not touch `/home/tarunvir/projects/lihaaf`

## Risk, Security, and Performance Notes

Correctness risks:

- Dual state is the main avoidable risk. If `clauses` and `q_leaves` diverge, the database query and cache predicate can disagree. Single-source lazy mapping avoids that class of bug.
- String pattern semantics are not obviously identical between legacy `Condition` and portable predicate SQL. Fallback by default avoids an accidental behavior change.
- `FilterValue` is non-exhaustive. The mapper must include graceful fallback for unknown/future variants.
- Optional/null mapping must only be generated for nullable/optional fields.

Security risks:

- Do not introduce raw SQL interpolation. Values must remain bound through existing `FilterValue` / SQL emitter paths.
- Identifiers must remain macro-baked and typed/validated. Do not accept arbitrary user strings as portable field identifiers.
- Do not create a public arbitrary `sassi::BasicPredicate` ingress. Any hidden helper must mint provenance inside `djogi` and validate against model metadata.
- Keep sealed raw SQL proxy fragments sealed and out of scope.

Performance risks:

- Lazy conversion is O(n) at `IntoQ` consumption time and avoids double allocation on every setter.
- Consume `FilterClause` values rather than cloning large `FilterValue`s.
- Prefer generated `match` tables over runtime reflection or string maps.
- Avoid broad SQL emission changes that could degrade query planner behavior.

Scalability/API risks:

- New public API surface increases docs/support burden. Reuse `filter_struct` unless the owner explicitly asks for an alias.
- Keep the generated mapper driven by `portable_field_info`; duplicated classification will drift as new field kinds are added.
- Treat proxy default filter portability as a separate design because it affects the `Model` trait contract.

## Review Gates

Pre-implementation design gate:

- Confirm owner defaults for string patterns, proxy defaults, partial pushdown, and public API naming.
- Review the exact portable mapping matrix before writing code.

Implementation review focus:

- Generated filters still store only `clauses`.
- Generated `IntoQ` uses the shared `portable_field_info` classifier.
- Unsupported fields/ops/values fallback to `Q::Condition`, not panic.
- Empty fold is `Q::always_true()`.
- Mixed generated filters remain all-or-nothing at cache/refresh boundaries.
- No raw SQL or raw `sassi::BasicPredicate` public ingress is added.
- Docs are updated before issue closure.

Regression review focus:

- Existing closure `.filter(|f| ...)` Q portability is unchanged.
- Existing `filter_struct` row-set behavior is preserved.
- SQL-only JSONB, regex, expression, and proxy filters still reject cache/refresh.
- `exclude_struct` semantics remain coherent for empty and portable generated filters.
- String pattern behavior matches the owner-approved policy and fixtures.

Rollback checkpoints:

- Helper addition in `djogi/src/query/filter.rs` is additive.
- Macro signature change is local to model expansion.
- Generated `IntoQ` body is the primary behavior switch and can be reverted independently.
- Reducer should not need rollback because it should not change.
- Docs/tests can be reverted or narrowed if the mapped op set is narrowed.

## Interaction Notes With #161 and JSONB/Q Algebra

#126 should not solve JSONB path portability.

Current facts:

- JSONB path comparisons return `Condition`.
- `Q::JsonbPath` exists but cache/refresh reduction rejects it.
- `PortableFieldKind::Jsonb` is SQL-only in macro field classification.
- #161 is about custom `primary_key!` newtype JSONB path cast dispatch, not about cache predicate portability.

Recommended interaction policy:

- Keep JSONB and JSONB path clauses as fallback/nonportable in #126.
- Do not reuse the `FilterClause -> Q` mapper for JSONB typed path cast dispatch.
- If #161 changes JSONB path SQL casts, keep `Q::JsonbPath` nonportable unless a separate Punnu-equivalence design proves safe reduction.
- #126 can land before or after #161. The likely conflict surface is docs/tests that mention JSONB cache behavior; keep those statements explicit: JSONB path filters are SQL-only at the cache boundary.

Q-algebra path:

- Current direct-Q composition through closure filters is already fixed for ordinary typed fields.
- #126 extends that same path to generated `ModelFilter` without changing reducer semantics.
- The correct mental model after #126 is:
  - closure portable filters produce Q directly
  - generated filters lazily reconstruct Q from typed/validated clauses where safe
  - SQL-only clauses remain explicit fallback nodes
  - cache/refresh admits only fully reducible Q trees

## Final Recommended Landing Shape

Land #126 as a narrowly scoped generated-filter portability fix:

1. Add a clause-to-Q fold helper.
2. Feed `portable_field_info` into filter macro generation.
3. Generate lazy single-source `{Model}Filter -> Q<Model>` conversion.
4. Keep unsupported clauses as `Q::Condition`.
5. Leave reducer all-or-nothing.
6. Add focused integration tests proving generated portable filters reduce and refresh, while mixed filters reject.
7. Update rustdoc, guide, spec, and examples as required by the issue owner.

Do not land dual `clauses + q_leaves` state. It is the less safe design because it creates a representation split exactly at the database/cache boundary this issue is trying to make trustworthy.
