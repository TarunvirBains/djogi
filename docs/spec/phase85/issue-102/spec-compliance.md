# Issue #102 Spec Compliance Report

Date: 2026-05-21 (current environment date)

## Scope

- Branch/worktree: `.git/worktrees/issue-102-lateral-joins`
- Source under review: typed LATERAL joins implementation (`djogi/src/query/lateral.rs`, queryset API, outer-ref helpers, SQL tests).

## Checks Run

- `rtk cargo fmt --all -- --check`
- `rtk cargo test -p djogi --test phase8_5_c4b_lateral_sql_shape --features testing`
- `DATABASE_URL=postgres://djogi:djogi@localhost:5432/djogi_test rtk proxy cargo test -p djogi --test phase8_5_c4b_lateral_join_live --features testing -- --nocapture --test-threads=1`

## Results

- `cargo fmt --check`: **passed**
- `phase8_5_c4b_lateral_sql_shape`: **passed (4 passed / 0 failed)**
- `phase8_5_c4b_lateral_join_live`: **passed (3 passed / 0 failed)**

## Spec/Decision Alignment Notes

1. API surface present and exported:
   - `QuerySet::join_lateral(...)`
   - `QuerySet::left_join_lateral(...)`
2. Result tuple shapes align with owner decision:
   - inner LATERAL: `(L, R)`
   - left LATERAL: `(L, Option<R>)`
3. Typed correlation helper present:
   - `OuterRef::as_lateral_outer_expr()` emits `l.<column>`.
4. Join predicate policy aligns:
   - structural join emitted as `ON TRUE`.
5. Inner modifiers behavior:
   - inner `WHERE`, `ORDER BY`, `LIMIT`, `OFFSET` preserved.
   - inner `DISTINCT` now preserved (`DISTINCT` / `DISTINCT ON (...)` emission in lateral inner query).
6. Unsupported state rejects explicitly:
   - prefetch, select-related, cache target, row locks.
7. Tenant setup propagated for both outer and inner models before execution.
8. `count()` wraps lateral select and strips only outer result-ordering/limit/offset; inner modifiers remain in place.

## Verdict

- Current `issue-102-lateral-joins` branch state is compliant with the recorded owner decision in `docs/spec/phase85/issue-102/owner-decision.md`.
