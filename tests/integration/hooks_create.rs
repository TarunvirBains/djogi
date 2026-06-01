// T1.4 integration tests: `before_create` + `after_create`
// dispatch around the macro-emitted `Model::create()` body.
//
// What this file pins:
//
// 1. `before_create(&mut value, ctx)` fires before the INSERT and may
//    mutate the in-memory value — the mutation round-trips through the
//    `RETURNING` clause back into the row the caller receives.
// 2. `after_create(&row, ctx)` fires after the INSERT — the hook can
//    use typed model APIs to observe the just-inserted row.
// 3. Returning `Err` from `before_create` short-circuits the entire
//    sequence: no INSERT lands, and a follow-up `objects().count()`
//    confirms zero rows landed.
//
// §D3 lines 118-129 fix the canonical sequence as
// `before_create -> INSERT -> outbox -> after_create -> on_commit drain`.
// Order is load-bearing: T1.7 will add the events-model variant that
// also asserts the outbox row exists by the time `after_create` runs.
//
// # One model per test — coherence
//
// `impl ModelHooks for T` is a coherent impl: only one per `T` per
// crate. Each test therefore declares its own model type
// (`MutateCounter`, `ObserveCounter`, `AbortCounter`) sharing a single
// `hook_counters_*` table shape. The model name is a load-bearing
// disambiguator — without it the three tests' hook bodies would
// conflict at the trait-impl level.
//
// # Fixture strategy
//
// Each test provisions its models through `sync_models`, so the body
// exercises the same typed/spec surface an adopter would use. Tokio
// task-locals carry per-test cross-hook state where needed;
// `#[djogi_test]` runs each test on its own per-test database, so
// cross-test pollution is impossible.

use djogi::prelude::*;
use std::cell::Cell;

// ---------------------------------------------------------------------------
// Test 1 — before_create can mutate `value` before the INSERT composes.
// ---------------------------------------------------------------------------

#[model(table = "mutate_counters", pk = HeerId, hooks)]
#[derive(Debug, Clone)]
pub struct MutateCounter {
    pub value: i32,
}

impl djogi::hooks::ModelHooks for MutateCounter {
    async fn before_create(
        &mut self,
        _ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        self.value = 42;
        Ok(())
    }
}

#[djogi::djogi_test(sync_models = [MutateCounter])]
async fn before_create_fires_and_can_mutate_value(mut ctx: djogi::DjogiContext) {
    let row = MutateCounter::create(
        &mut ctx,
        MutateCounter {
            value: 0,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed and run before_create");

    assert_eq!(
        row.value, 42,
        "before_create's mutation must round-trip through RETURNING into the returned row",
    );

    // And the row must actually be on disk with the mutated value —
    // proves the INSERT saw the post-hook value, not the pre-hook one.
    let on_disk = MutateCounter::get(&mut ctx, row.id)
        .await
        .expect("typed get should round-trip the inserted value");
    assert_eq!(
        on_disk.value, 42,
        "DB row must reflect the hook-mutated value"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — after_create observes the just-inserted row.
//
// `before_create` flips a Tokio task-local `Cell` so the test body can
// confirm the hook ran; `after_create` re-reads the row through the
// typed model API to prove it is queryable from inside the hook.
// ---------------------------------------------------------------------------

#[model(table = "observe_counters", pk = HeerId, hooks)]
#[derive(Debug, Clone)]
pub struct ObserveCounter {
    pub value: i32,
}

// Tokio task-locals are async-aware and survive `.await` points inside
// hook bodies. `#[djogi_test]` runs each test on its own task so these
// `Cell`s are private to this single test.
tokio::task_local! {
    static BEFORE_FIRED: Cell<bool>;
    static AFTER_FIRED: Cell<bool>;
    static AFTER_OBSERVED_VALUE: Cell<i32>;
}

impl djogi::hooks::ModelHooks for ObserveCounter {
    async fn before_create(
        &mut self,
        _ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        BEFORE_FIRED.with(|c| c.set(true));
        Ok(())
    }

    async fn after_create(&self, ctx: &mut djogi::DjogiContext) -> Result<(), djogi::DjogiError> {
        // Re-fetch the row through typed CRUD from inside the hook.
        // If `after_create` runs after the INSERT (per §D3), the
        // row must be visible to the active ctx.
        let observed = ObserveCounter::get(ctx, self.id).await?;
        AFTER_OBSERVED_VALUE.with(|c| c.set(observed.value));
        AFTER_FIRED.with(|c| c.set(true));
        Ok(())
    }
}

#[djogi::djogi_test(sync_models = [ObserveCounter])]
async fn after_create_observes_inserted_row(mut ctx: djogi::DjogiContext) {
    BEFORE_FIRED
        .scope(Cell::new(false), async {
            AFTER_FIRED
                .scope(Cell::new(false), async {
                    AFTER_OBSERVED_VALUE
                        .scope(Cell::new(-1), async {
                            let row = ObserveCounter::create(
                                &mut ctx,
                                ObserveCounter {
                                    value: 7,
                                    ..Default::default()
                                },
                            )
                            .await
                            .expect("create should succeed");

                            assert!(
                                BEFORE_FIRED.with(Cell::get),
                                "before_create must fire on a hooks-enabled model",
                            );
                            assert!(
                                AFTER_FIRED.with(Cell::get),
                                "after_create must fire on a hooks-enabled model",
                            );
                            assert_eq!(
                                AFTER_OBSERVED_VALUE.with(Cell::get),
                                7,
                                "after_create's typed get must observe the just-inserted row",
                            );
                            assert_eq!(
                                row.value, 7,
                                "create() must return the row populated from RETURNING",
                            );
                        })
                        .await
                })
                .await
        })
        .await;
}

// ---------------------------------------------------------------------------
// Test 3 — before_create returning Err aborts the entire sequence.
//
// The count after the failed `create()` must be zero because the hook
// aborts before the INSERT composes.
// ---------------------------------------------------------------------------

#[model(table = "abort_counters", pk = HeerId, hooks)]
#[derive(Debug, Clone)]
pub struct AbortCounter {
    pub value: i32,
}

impl djogi::hooks::ModelHooks for AbortCounter {
    async fn before_create(
        &mut self,
        _ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        Err(djogi::DjogiError::Validation("nope".into()))
    }
}

#[djogi::djogi_test(sync_models = [AbortCounter])]
async fn before_create_err_aborts_no_row(mut ctx: djogi::DjogiContext) {
    let res = AbortCounter::create(
        &mut ctx,
        AbortCounter {
            value: 1,
            ..Default::default()
        },
    )
    .await;

    let Err(djogi::DjogiError::Validation(msg)) = res else {
        panic!("expected Err(DjogiError::Validation(_)), got {res:?}");
    };
    assert_eq!(
        msg, "nope",
        "before_create's Err variant must propagate unchanged",
    );

    // The attempted INSERT never ran because before_create aborted via `?`.
    let count: i64 = AbortCounter::objects()
        .count(&mut ctx)
        .await
        .expect("count should succeed on the empty table");
    assert_eq!(
        count, 0,
        "before_create returning Err must leave the table empty",
    );
}

// ---------------------------------------------------------------------------
// Test 4 — before_create runs BEFORE any DB write, including the
// `#[field(sequence_within = ...)]` counter upsert.
//
// Adversarial-review counter-signal (T1 cluster review,
// Codex 2026-05-04 BLOCK-1): the macro previously emitted the
// sequence-counter upsert AHEAD of `before_create`, so an aborted
// hook on the test context would still increment
// the per-parent counter — leaking sequence numbers on validation
// failure. The fix moves `before_create` ahead of the upsert so the
// canonical sequence holds: `before -> DB -> outbox -> after`.
//
// This test exercises the invariant directly on the test context so
// an ordering regression cannot be hidden by a surrounding rollback.
// ---------------------------------------------------------------------------

#[model(table = "seq_abort_parents", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct SeqAbortParent {
    pub name: String,
}

// `no_default` because `ForeignKey<T>` is not `Default`. `hooks` opts into
// the dispatch path. `sequence_within` writes back into a `i64`
// field per the macro's `try_get::<i64>` decode of `last_seq`.
#[model(table = "seq_abort_children", pk = HeerId, hooks, no_default)]
#[derive(Debug, Clone)]
pub struct SeqAbortChild {
    pub parent_id: ForeignKey<SeqAbortParent>,
    #[field(sequence_within = "parent_id")]
    pub seq_num: i64,
}

impl djogi::hooks::ModelHooks for SeqAbortChild {
    async fn before_create(
        &mut self,
        _ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        Err(djogi::DjogiError::Validation("seq abort".into()))
    }
}

#[djogi::djogi_test(sync_models = [SeqAbortChild, SeqAbortParent])]
async fn before_create_err_blocks_sequence_within_upsert(mut ctx: djogi::DjogiContext) {
    let parent = SeqAbortParent::create(
        &mut ctx,
        SeqAbortParent {
            name: "p".into(),
            ..Default::default()
        },
    )
    .await
    .expect("parent insert");

    // Without the ordering fix, the counter upsert would happen
    // before the hook returned Err. `no_default` requires explicit framework-column
    // construction; `id` uses the PK sentinel and the row's `id` is
    // populated server-side via `RETURNING` (irrelevant here because
    // before_create aborts before the INSERT runs).
    let res = SeqAbortChild::create(
        &mut ctx,
        SeqAbortChild {
            id: HeerId::ZERO,
            created_at: time::OffsetDateTime::now_utc(),
            updated_at: time::OffsetDateTime::now_utc(),
            parent_id: ForeignKey::new(parent.id),
            seq_num: 0,
        },
    )
    .await;

    let Err(djogi::DjogiError::Validation(msg)) = res else {
        panic!("expected Err(Validation), got {res:?}");
    };
    assert_eq!(msg, "seq abort");

    let child_rows: i64 = SeqAbortChild::objects()
        .count(&mut ctx)
        .await
        .expect("count child rows");
    assert_eq!(
        child_rows, 0,
        "the aborted create must leave the child table empty",
    );
}
