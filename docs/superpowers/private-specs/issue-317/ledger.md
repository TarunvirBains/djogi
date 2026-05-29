<!--
ledger_slug: durable-ledger-2026-05-26-issue-317-partition-rollback-repair
source_spec: docs/superpowers/private-specs/issue-317/plan.md (lines 1-808)
authority_rank: primary — hardened private plan; supersedes issue body for implementation details
spec_shape: tight — single dense file with source-native REQ IDs
created: 2026-05-26
status: implemented
-->

# Durable Ledger — Issue #317 Preserve Partition-Expanded Rollback And Repair Replay

## Sources

| Rank | Source | Notes |
|------|--------|-------|
| 1 | `docs/superpowers/private-specs/issue-317/plan.md` | Hardened plan with REQ table, 7 tasks (T1-T7), scope guardrails |
| 2 | `djogi/src/migrate/runner.rs` (main) | Current partition expansion, `apply_plan_inner`, `rollback_plan_pinned`, `expand_partition_statement` — baseline |
| 3 | `djogi/src/migrate/repair.rs` (main) | Current repair logic, `repair_resume_body`, error types — baseline |
| 4 | `tests/internal/sources/phase7_t5_repair_verify_live.rs` | Existing repair live tests — regression floor |
| 5 | `tests/internal/sources/phase7_t9_pk_flip_live.rs` | Existing PK flip live tests — regression floor |

## Requirements Table

| ID | Requirement | Source | Authority | Ship | Status | Evidence |
|----|-------------|--------|-----------|------|--------|----------|
| REQ-1 | Add `PartitionExpansionMode` enum with `ApplyLenient` and `ReplayStrict` variants. Mode controls empty-leaf behavior: ApplyLenient falls back to no-op comment, ReplayStrict returns `RunnerError::PartitionExpansionNoLeaves`. | plan.md L146-150 | primary | must_ship | completed | d3398415 runner.rs L3557-3566 |
| REQ-2 | Add `materialize_execution_plan()` helper that wraps `expand_partition_leaf_placeholders` with mode parameter. Returns `MigrationPlan`. | plan.md L152-158 | primary | must_ship | completed | d3398415 runner.rs L3633-3639 |
| REQ-3 | Add `RunnerError::PartitionExpansionNoLeaves { parent: String, statement_label: String }` with Display including "partition expansion for `<label>` refused: partitioned parent `<parent>` has 0 leaves in replay-strict mode". | plan.md L162-167 | primary | must_ship | completed | d3398415 runner.rs enum variant + Display impl |
| REQ-4 | Change `expand_partition_statement` to return `Result<Vec<OperationSql>, RunnerError>`. Empty-leaves: ApplyLenient emits no-op comment, ReplayStrict returns error. Every non-empty return becomes `Ok(out)`. | plan.md L189-209 | primary | must_ship | completed | d3398415 runner.rs expand_partition_leaf_placeholders signature change |
| REQ-5 | Change `apply_plan_inner` to call `materialize_execution_plan(ctx, plan, PartitionExpansionMode::ApplyLenient)` instead of direct `expand_partition_leaf_placeholders`. | plan.md L170-177 | primary | must_ship | completed | d3398415 runner.rs apply_plan_inner materialization call |
| REQ-6 | `rollback_plan_pinned` materializes strict replay inside advisory lock: `materialize_execution_plan(ctx, plan, PartitionExpansionMode::ReplayStrict)`. Lossy scanning and reverse down execution operate on the expanded stream. | plan.md L708-749 | primary | must_ship | completed | 9062be6c runner.rs rollback_plan_pinned materialization block |
| REQ-7 | Add `RollbackError::Runner(RunnerError)` variant for materialization failure propagation. | plan.md L711 | primary | must_ship | completed | Pre-existing at runner.rs L1538 — no change needed |
| REQ-8 | Extract `rollback_lossy_allow_reason()` helper that scans lossy markers on the expanded plan and returns Result. Must work against `replay_plan`, not original `plan`. | plan.md L720-739 | primary | must_ship | completed | 9062be6c runner.rs new helper function |
| REQ-9 | Add `RepairError::ResumePlanShapeMismatch { version: String, ledger_total_steps: usize, replay_total_steps: usize }` with Display including version, ledger total_steps, expanded replay count. | plan.md L500-514 | primary | must_ship | completed | eff5bdb3 repair.rs new variant + Display impl |
| REQ-10 | Add `RepairError::ReplayPlanShapeMismatch { version: String, expected_step_count: usize, actual_step_count: usize }` with Error::source returning Some(source). | plan.md L505-509 | primary | must_ship | completed | eff5bdb3 repair.rs new variant + Display impl (note: renamed from ReplayPlanMaterializationFailed to match finalization guard semantics) |
| REQ-11 | Add `count_non_transactional_statements(plan: &MigrationPlan) -> i32` helper in repair.rs. Counts only NonTransactional segment statements with saturating_add. | plan.md L521-527 | primary | must_ship | completed | eff5bdb3 repair.rs helper used in materialization flow |
| REQ-12 | `repair_resume_body` materializes strict replay inside advisory lock: `materialize_execution_plan(ctx, plan, PartitionExpansionMode::ReplayStrict)`. Validates expanded count equals ledger total_steps before proceeding. | plan.md L539-553 | primary | must_ship | completed | eff5bdb3 repair.rs repair_resume_body materialization + validation block |
| REQ-13 | Repair refuses to finalize if `applied != total` after replay loop completes (finalization guard). Returns `ResumePlanShapeMismatch`. | plan.md L560-567 | primary | must_ship | completed | eff5bdb3 repair.rs finalization guard after replay loop |
| REQ-14 | Repair checksum validation operates on original unexpanded `plan` BEFORE materialization. Checksum must match row.checksum_up before strict expansion. | plan.md L531-535 | primary | must_ship | completed | eff5bdb3 repair.rs — checksum validated before materialization call |

## Test Requirements

| ID | Test Name | Type | Source | Status | Evidence |
|----|-----------|------|--------|--------|----------|
| TEST-1 | `expand_partition_statement_partitioned_index_preserves_leaf_drop_down_sql` | Unit | plan.md L37-79 | completed | d5954370 runner.rs test — PASS |
| TEST-2 | `expand_partition_leaf_placeholders_replay_mode_refuses_empty_leaves` | Unit | plan.md L86-124 | completed | d5954370 runner.rs test — PASS |
| TEST-3 | `repair_error_resume_plan_shape_mismatch_display_names_counts` | Unit | plan.md L234-245 | completed | 8020a8b7 repair.rs test — PASS |
| TEST-4 | `repair_resume_partial_apply_refuses_when_replay_stream_is_shorter_than_total_steps` | Live | plan.md L251-341 | completed | 8020a8b7 phase7_t5_repair_verify_live.rs — compiles clean, runtime requires DATABASE_URL |
| TEST-5 | `flip_partitioned_parent_partial_apply_resume_uses_expanded_leaf_steps` | Live | plan.md L350-471 | completed | 8020a8b7 phase7_t9_pk_flip_live.rs — compiles clean, runtime requires DATABASE_URL |
| TEST-6 | `flip_partitioned_parent_rollback_uses_expanded_leaf_down_sql` | Live | plan.md L591-675 | completed | d22ac416 phase7_t9_pk_flip_live.rs — compiles clean, runtime requires DATABASE_URL |
| TEST-7 | `index_exists_by_name` helper (if not present) | Helper | plan.md L680-687 | completed | d22ac416 phase7_t9_pk_flip_live.rs helper function |

## Non-Goals

| ID | Description | Source |
|----|-------------|--------|
| NG1 | No changes to `djogi/src/migrate/pk_flip.rs` emission | plan.md L14, L26 |
| NG2 | No changes to migration checksum identity or preflight policy | plan.md L14 |
| NG3 | No use of `djogi/src/migrate/replay_plan.rs` or `djogi/src/migrate/reset.rs` | plan.md L15, L26 |
| NG4 | No apply-time persisted expanded manifest in this issue | plan.md L16 |
| NG5 | Do not compare nonexistent `MigrationPlan.version`; repair uses explicit version arg + original checksum_up | plan.md L17 |
| NG6 | Preserve apply's current empty-leaf fallback only for apply mode; strict replay refuses same shape | plan.md L18 |

## Deferral Log

| Date | REQ ID | Reason | Substitute | Gap |
|------|--------|--------|------------|-----|
| (none) | — | — | — | — |

## Completion Criteria

All 14 REQ items and 7 TEST items are `completed` with passing verification gates. Final verification:

1. Unit tests (REQ-1,2,3,9,11): `cargo test -p djogi migrate::runner::tests::expand_partition_statement_partitioned_index_preserves_leaf_drop_down_sql -- --exact` — **PASS**
2. Unit tests (REQ-4,10): `cargo test -p djogi migrate::runner::tests::expand_partition_leaf_placeholders_replay_mode_refuses_empty_leaves -- --exact` — **PASS**
3. Unit test (REQ-9): `cargo test -p djogi migrate::repair::tests::repair_error_resume_plan_shape_mismatch_display_names_counts -- --exact` — **PASS**
4. Live tests (TEST-4,5,6): Compile clean — **PASS** (runtime requires DATABASE_URL)
5. Regression: 2226 unit tests pass, lihaaf 49 fixtures OK — **PASS**
6. `cargo fmt --check` — **PASS**
7. `cargo clippy -p djogi --all-targets --all-features` — **PASS** (0 new warnings, 2 pre-existing large-Err-variant warnings)

## Stage Execution State

| Stage | Status | Started | Completed | Evidence |
|-------|--------|---------|-----------|----------|
| T1 — Runner unit tests (TEST-1, TEST-2) | completed | 2026-05-26 | 2026-05-26 | d5954370 |
| T2 — Shared materialization helper + strict mode (REQ-1-5) | completed | 2026-05-26 | 2026-05-26 | d3398415 |
| T3 — Repair shape-mismatch tests (TEST-3, TEST-4, TEST-5 imports) | completed | 2026-05-26 | 2026-05-26 | 8020a8b7 |
| T4 — Strict repair replay + finalization guard (REQ-9-14) | completed | 2026-05-26 | 2026-05-26 | eff5bdb3 |
| T5 — Partitioned rollback live test (TEST-6, TEST-7) | completed | 2026-05-26 | 2026-05-26 | d22ac416 |
| T6 — Strict rollback replay against materialized plan (REQ-6-8) | completed | 2026-05-26 | 2026-05-26 | 9062be6c |
| T7 — Final verification | completed | 2026-05-26 | 2026-05-26 | All gates pass (see Completion Criteria above) |

## Branch & Commits

Branch: `beta-blocker/issue-317-partition-rollback`
Worktree: `.worktrees/beta-blocker/issue-317-partition-rollback/`

```
9062be6c feat(migrate/runner): strict rollback replay via materialized plan (#317 T6)
d22ac416 test(migrate): add partitioned rollback live test and index_exists helper (#317 T5)
eff5bdb3 feat(migrate/repair): strict materialization and shape-mismatch guards (#317 T4)
8020a8b7 test(migrate): add ResumePlanShapeMismatch and partition-resume red tests (#317 T3)
d3398415 feat(migrate/runner): add partition expansion mode and materialization helper (#317 T2)
d5954370 test(runner): add partition replay materialization unit tests (#317 T1)
6ff25cf0 fix(migrate): preserve phase-zero replay guard (origin/main HEAD)
```
