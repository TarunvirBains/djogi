# Issue #126 Owner Decision

Recorded: 2026-05-20 23:34 MDT.

Decision: approved Option 2 — lazy single-source conversion.

Approved #126 direction:

- Keep generated `{Model}Filter` storage as `clauses: Vec<FilterClause>`.
- Do not add `q_leaves`, cached Q leaves, or any other parallel predicate
 representation.
- Convert `FilterClause -> Q<Model>` lazily inside generated
 `{Model}Filter: IntoQ<Model>` at consumption time.
- Empty filters fold to `Q::always_true()`.
- Portable scalar equality/order/null/list clauses may become `Q::Portable`.
- Unsupported fields, unsupported operations, value-shape mismatches, JSONB,
 spatial, FTS, and other SQL-only cases must gracefully fall back to
 `Q::Condition`.
- Preserve all-or-nothing cache/refresh admission. If any nonportable residual
 remains, the whole query is not cache-pushdown eligible.
- Do not design partial pushdown/residual cache evaluation in #126.
- Keep proxy default filters out of #126.
- Do not add a new public `.filter_q()` API; keep `filter_struct` as the
 public entry point for `ModelFilter` and prebuilt `Q`.
- Keep JSONB path predicates SQL-only for #126.
- Keep string pattern lookups (`contains`, `starts_with`, `ends_with`) on the
 conservative fallback-to-`Q::Condition` path unless/until portable
 `COLLATE "C" ILIKE ... ESCAPE` semantics are approved in a separate,
 explicit issue.

Implementation note: implementation workers must treat dual state as rejected,
not merely deferred.
