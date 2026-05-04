//! Phase 8α T1.4 integration tests: `before_create` + `after_create`
//! dispatch around the macro-emitted `Model::create()` body.
//!
//! What this file pins:
//!
//! 1. `before_create(&mut value, ctx)` fires before the INSERT and may
//!    mutate the in-memory value — the mutation round-trips through the
//!    `RETURNING` clause back into the row the caller receives.
//! 2. `after_create(&row, ctx)` fires after the INSERT (and after the
//!    outbox emission, though this fixture is non-events) — the hook can
//!    issue `ctx.raw_*` queries that observe the just-inserted row.
//! 3. Returning `Err` from `before_create` short-circuits the entire
//!    sequence: no INSERT, no outbox row. Wrapped in `atomic()`, the
//!    surrounding transaction rolls back via standard `?` propagation
//!    and a follow-up `objects().count()` confirms zero rows landed.
//!
//! Phase 8 §D3 lines 118-129 fix the canonical sequence as
//! `before_create -> INSERT -> outbox -> after_create -> on_commit drain`.
//! Order is load-bearing: T1.7 will add the events-model variant that
//! also asserts the outbox row exists by the time `after_create` runs.
//!
//! # One model per test — coherence
//!
//! `impl ModelHooks for T` is a coherent impl: only one per `T` per
//! crate. Each test therefore declares its own model type
//! (`MutateCounter`, `ObserveCounter`, `AbortCounter`) sharing a single
//! `hook_counters_*` table shape. The model name is a load-bearing
//! disambiguator — without it the three tests' hook bodies would
//! conflict at the trait-impl level.
//!
//! # Fixture strategy
//!
//! Each test provisions its own table inline via `ctx.raw_execute(...)`.
//! `#[djogi::djogi_test]` already installs HeeRanjID schema, seeds node 1,
//! and sets `heer.node_id = '1'` before the test body runs — no manual
//! bootstrap needed beyond DDL. Tokio task-locals carry per-test cross-
//! hook state where needed; `#[djogi_test]` runs each test on its own
//! per-test database, so cross-test pollution is impossible.

use djogi::prelude::*;
use djogi::transaction::atomic;
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

async fn setup_mutate_counters(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE mutate_counters (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            value       INTEGER     NOT NULL
        )",
        &[],
    )
    .await
    .expect("create mutate_counters table");
}

#[djogi::djogi_test]
async fn before_create_fires_and_can_mutate_value(mut ctx: djogi::DjogiContext) {
    setup_mutate_counters(&mut ctx).await;

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
    let on_disk: i32 = ctx
        .raw_scalar(
            "SELECT value FROM mutate_counters WHERE id = $1",
            &[&row.id],
        )
        .await
        .expect("scalar select should round-trip the inserted value");
    assert_eq!(on_disk, 42, "DB row must reflect the hook-mutated value");
}

// ---------------------------------------------------------------------------
// Test 2 — after_create observes the just-inserted row.
//
// `before_create` flips a Tokio task-local `Cell` so the test body can
// confirm the hook ran; `after_create` re-reads the row through
// `ctx.raw_scalar` to prove it is queryable from inside the hook (i.e.
// the INSERT has committed to the active connection's view).
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
        // Re-fetch the row through a raw_scalar from inside the hook.
        // If `after_create` runs after the INSERT (per Phase 8 §D3), the
        // row must be visible to the active ctx.
        let observed: i32 = ctx
            .raw_scalar(
                "SELECT value FROM observe_counters WHERE id = $1",
                &[&self.id],
            )
            .await?;
        AFTER_OBSERVED_VALUE.with(|c| c.set(observed));
        AFTER_FIRED.with(|c| c.set(true));
        Ok(())
    }
}

async fn setup_observe_counters(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE observe_counters (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            value       INTEGER     NOT NULL
        )",
        &[],
    )
    .await
    .expect("create observe_counters table");
}

#[djogi::djogi_test]
async fn after_create_observes_inserted_row(mut ctx: djogi::DjogiContext) {
    setup_observe_counters(&mut ctx).await;

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
                                "after_create's raw_scalar must observe the just-inserted row",
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
// Wrapped in `atomic()` so the rollback on Err is observable: the count
// after the failed `create()` must be zero.
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

async fn setup_abort_counters(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE abort_counters (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            value       INTEGER     NOT NULL
        )",
        &[],
    )
    .await
    .expect("create abort_counters table");
}

#[djogi::djogi_test]
async fn before_create_err_aborts_no_row(mut ctx: djogi::DjogiContext) {
    setup_abort_counters(&mut ctx).await;
    let pool = ctx.pool().expect("djogi_test ctx is pool-backed").clone();

    let res: Result<(), djogi::DjogiError> = atomic(&pool, |ctx| {
        Box::pin(async move {
            AbortCounter::create(
                ctx,
                AbortCounter {
                    value: 1,
                    ..Default::default()
                },
            )
            .await?;
            // Should never reach here — before_create's Err must short-
            // circuit the create() body before the INSERT.
            Ok(())
        })
    })
    .await;

    let Err(djogi::DjogiError::Validation(msg)) = res else {
        panic!("expected Err(DjogiError::Validation(_)), got {res:?}");
    };
    assert_eq!(
        msg, "nope",
        "before_create's Err variant must propagate unchanged",
    );

    // The atomic() scope rolled back. The inner-attempted INSERT never
    // ran (before_create aborted via `?`), but even if it had, the
    // rollback would clean it up. Either way: zero rows.
    let count: i64 = AbortCounter::objects()
        .count(&mut ctx)
        .await
        .expect("count should succeed on the empty table");
    assert_eq!(
        count, 0,
        "before_create returning Err must leave the table empty (no INSERT, atomic rolls back)",
    );
}
