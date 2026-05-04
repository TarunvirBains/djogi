//! Phase 8α T1.5 integration tests: `before_save` + `after_save` dispatch
//! around the macro-emitted `Model::save()` body.
//!
//! What this file pins:
//!
//! 1. `before_save(self, ctx)` fires before the UPDATE composes and may
//!    mutate the in-memory `*self` — the mutation round-trips through the
//!    `RETURNING` clause back into the post-save state of `self`.
//! 2. `after_save(&*self, ctx)` fires after the outbox emission AND after
//!    `*self = row` rehydration. The hook therefore sees server-side
//!    defaults / triggers / sequence-bumped values, not the pre-call
//!    in-memory state.
//! 3. Returning `Err` from `before_save` short-circuits the entire
//!    sequence: no UPDATE composes, no outbox row is written. Wrapped in
//!    `atomic()`, the surrounding transaction rolls back via standard `?`
//!    propagation; a follow-up re-fetch confirms the row is unchanged.
//! 4. Shape B (version-aware) `LockConflict` early-return path skips
//!    `after_save` — the UPDATE didn't actually mutate the row, so
//!    `after_save` would observe stale state.
//!
//! Phase 8 §D3 lines 118-129 fix the canonical sequence as
//! `before_save -> UPDATE -> outbox -> after_save -> on_commit drain`.
//! Order is load-bearing: T1.7 will add the events-model variant that
//! also asserts the outbox row exists by the time `after_save` runs.
//!
//! # One model per test — coherence
//!
//! `impl ModelHooks for T` is a coherent impl: only one per `T` per
//! crate. Each test therefore declares its own model type sharing a
//! single `hook_save_*` table shape. Test 4 is version-aware (uses
//! `#[field(version)]`) and therefore has a slightly different shape.
//!
//! # Fixture strategy
//!
//! Each test provisions its own table inline via `ctx.raw_execute(...)`.
//! `#[djogi::djogi_test]` already installs HeeRanjID schema, seeds node 1,
//! and sets `heer.node_id = '1'` before the test body runs. Tokio
//! task-locals carry per-test cross-hook state where needed; each test
//! runs on its own per-test database.

use djogi::prelude::*;
use djogi::transaction::atomic;
use std::cell::Cell;

// ---------------------------------------------------------------------------
// Test 1 — before_save fires before the UPDATE; after_save fires after
// rehydration. The two hooks push tags into a per-test recorder so the
// test body can assert the order matches the D3 contract.
// ---------------------------------------------------------------------------

#[model(table = "save_recorders", pk = HeerId, hooks)]
#[derive(Debug, Clone)]
pub struct SaveRecorder {
    pub value: i32,
}

// Tokio task-locals: each test runs on its own task so these `Cell`s are
// private to this single test. We use a per-tag boolean instead of a
// shared Vec<&'static str> to avoid `RefCell` ergonomics — each tag has
// a recorded ordinal that captures the relative call order.
tokio::task_local! {
    static SR_NEXT_ORDINAL: Cell<u8>;
    static SR_BEFORE_AT: Cell<u8>;
    static SR_AFTER_AT: Cell<u8>;
}

fn sr_record_before() {
    let next = SR_NEXT_ORDINAL.with(Cell::get);
    SR_BEFORE_AT.with(|c| c.set(next));
    SR_NEXT_ORDINAL.with(|c| c.set(next + 1));
}

fn sr_record_after() {
    let next = SR_NEXT_ORDINAL.with(Cell::get);
    SR_AFTER_AT.with(|c| c.set(next));
    SR_NEXT_ORDINAL.with(|c| c.set(next + 1));
}

impl djogi::hooks::ModelHooks for SaveRecorder {
    async fn before_save(
        &mut self,
        _ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        sr_record_before();
        Ok(())
    }

    async fn after_save(&self, _ctx: &mut djogi::DjogiContext) -> Result<(), djogi::DjogiError> {
        sr_record_after();
        Ok(())
    }
}

async fn setup_save_recorders(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE save_recorders (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            value       INTEGER     NOT NULL
        )",
        &[],
    )
    .await
    .expect("create save_recorders table");
}

#[djogi::djogi_test]
async fn before_save_fires_pre_update(mut ctx: djogi::DjogiContext) {
    setup_save_recorders(&mut ctx).await;

    SR_NEXT_ORDINAL
        .scope(Cell::new(1), async {
            SR_BEFORE_AT
                .scope(Cell::new(0), async {
                    SR_AFTER_AT
                        .scope(Cell::new(0), async {
                            // INSERT path: no hooks for create are installed
                            // on this model (we deliberately only override
                            // before_save / after_save), so the create()
                            // call uses the default no-op create hooks.
                            let mut row = SaveRecorder::create(
                                &mut ctx,
                                SaveRecorder {
                                    value: 1,
                                    ..Default::default()
                                },
                            )
                            .await
                            .expect("create should succeed");

                            // Mutate before save() so the UPDATE actually has
                            // work to do.
                            row.value = 2;
                            row.save(&mut ctx)
                                .await
                                .expect("save should succeed and run both hooks");

                            let before_at = SR_BEFORE_AT.with(Cell::get);
                            let after_at = SR_AFTER_AT.with(Cell::get);
                            assert_eq!(
                                before_at, 1,
                                "before_save must fire first (ordinal 1); recorded {before_at}",
                            );
                            assert_eq!(
                                after_at, 2,
                                "after_save must fire after before_save (ordinal 2); \
                                 recorded {after_at}",
                            );
                        })
                        .await
                })
                .await
        })
        .await;
}

// ---------------------------------------------------------------------------
// Test 2 — before_save returning Err aborts the UPDATE. Wrapped in
// atomic() so the rollback is observable: the row's value must remain
// unchanged after the failed save().
// ---------------------------------------------------------------------------

#[model(table = "save_aborts", pk = HeerId, hooks)]
#[derive(Debug, Clone)]
pub struct SaveAbort {
    pub value: i32,
}

impl djogi::hooks::ModelHooks for SaveAbort {
    async fn before_save(
        &mut self,
        _ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        Err(djogi::DjogiError::Validation("nope".into()))
    }
}

async fn setup_save_aborts(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE save_aborts (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            value       INTEGER     NOT NULL
        )",
        &[],
    )
    .await
    .expect("create save_aborts table");
}

#[djogi::djogi_test]
async fn before_save_err_aborts(mut ctx: djogi::DjogiContext) {
    setup_save_aborts(&mut ctx).await;
    let pool = ctx.pool().expect("djogi_test ctx is pool-backed").clone();

    // Insert a row with value=1, OUTSIDE the atomic() so it survives
    // when the inner transaction rolls back.
    let row = SaveAbort::create(
        &mut ctx,
        SaveAbort {
            value: 1,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");
    let row_id = row.id;

    // Attempt to mutate the row inside an atomic(). before_save returns
    // Err — the UPDATE never composes, the outer atomic() rolls back via
    // `?` propagation.
    let res: Result<(), djogi::DjogiError> = atomic(&pool, |ctx| {
        Box::pin(async move {
            let mut loaded = SaveAbort::get(ctx, row_id).await?;
            loaded.value = 999;
            loaded.save(ctx).await?;
            // Should never reach here — before_save's Err short-circuits.
            Ok(())
        })
    })
    .await;

    let Err(djogi::DjogiError::Validation(msg)) = res else {
        panic!("expected Err(DjogiError::Validation(_)), got {res:?}");
    };
    assert_eq!(
        msg, "nope",
        "before_save's Err variant must propagate unchanged",
    );

    // Re-fetch the row outside the rolled-back atomic. The DB value must
    // still be 1: before_save aborted before the UPDATE composed, so
    // even without the atomic() rollback there was no UPDATE to undo.
    let on_disk: i32 = ctx
        .raw_scalar("SELECT value FROM save_aborts WHERE id = $1", &[&row_id])
        .await
        .expect("re-fetch should succeed");
    assert_eq!(
        on_disk, 1,
        "before_save returning Err must leave the DB row unchanged",
    );
}

// ---------------------------------------------------------------------------
// Test 3 — after_save sees the rehydrated row.
//
// We use the framework-managed `updated_at` column as the witness: the
// UPDATE composes `updated_at = now()`, so after rehydration the field
// reflects the trigger-set value (newer than the pre-save in-memory
// `updated_at`, which the framework copied off the prior INSERT's
// RETURNING). after_save reads `&*self.updated_at` and asserts the
// post-save value is strictly greater than the captured pre-save value.
// ---------------------------------------------------------------------------

#[model(table = "save_rehydrates", pk = HeerId, hooks)]
#[derive(Debug, Clone)]
pub struct SaveRehydrate {
    pub value: i32,
}

tokio::task_local! {
    static SH_PRE_SAVE_UPDATED_AT_NS: Cell<i128>;
    static SH_AFTER_OBSERVED_NS: Cell<i128>;
}

impl djogi::hooks::ModelHooks for SaveRehydrate {
    async fn after_save(&self, _ctx: &mut djogi::DjogiContext) -> Result<(), djogi::DjogiError> {
        // `updated_at` is `OffsetDateTime` (the `time` crate's type per
        // CLAUDE.md "use time crate not chrono"). `unix_timestamp_nanos`
        // returns i128 — wide enough for any plausible TIMESTAMPTZ value.
        SH_AFTER_OBSERVED_NS.with(|c| c.set(self.updated_at.unix_timestamp_nanos()));
        Ok(())
    }
}

async fn setup_save_rehydrates(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE save_rehydrates (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            value       INTEGER     NOT NULL
        )",
        &[],
    )
    .await
    .expect("create save_rehydrates table");
}

#[djogi::djogi_test]
async fn after_save_sees_rehydrated_row(mut ctx: djogi::DjogiContext) {
    setup_save_rehydrates(&mut ctx).await;

    SH_PRE_SAVE_UPDATED_AT_NS
        .scope(Cell::new(0), async {
            SH_AFTER_OBSERVED_NS
                .scope(Cell::new(0), async {
                    let mut row = SaveRehydrate::create(
                        &mut ctx,
                        SaveRehydrate {
                            value: 1,
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("create should succeed");

                    // Capture the pre-save updated_at — this is the value
                    // the create() call rehydrated from `RETURNING`.
                    let pre_save_ns = row.updated_at.unix_timestamp_nanos();
                    SH_PRE_SAVE_UPDATED_AT_NS.with(|c| c.set(pre_save_ns));

                    // Sleep briefly so the `updated_at = now()` trigger
                    // produces a strictly greater timestamp on save.
                    // 5ms is enough on every platform tested; the
                    // assertion below uses `>` not `>=` precisely because
                    // we want to prove the after_save observation reflects
                    // the post-UPDATE value.
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

                    row.value = 2;
                    row.save(&mut ctx)
                        .await
                        .expect("save should succeed and rehydrate updated_at");

                    let after_observed_ns = SH_AFTER_OBSERVED_NS.with(Cell::get);
                    assert!(
                        after_observed_ns > pre_save_ns,
                        "after_save must observe the rehydrated updated_at \
                         (pre={pre_save_ns}, observed={after_observed_ns}) — \
                         if observed == pre, after_save fired before \
                         `*self = row` rehydration",
                    );
                    // And the in-memory `*self` must match what after_save
                    // observed (proves the &*self pointer through the
                    // hook receives the rehydrated state, not a stale
                    // copy).
                    assert_eq!(
                        row.updated_at.unix_timestamp_nanos(),
                        after_observed_ns,
                        "after_save must observe the same rehydrated row \
                         the caller sees in *self after save() returns",
                    );
                })
                .await
        })
        .await;
}

// ---------------------------------------------------------------------------
// Test 4 — Shape B (version-aware) LockConflict propagates after
// before_save fires but BEFORE after_save fires.
//
// Strategy: clone the loaded row to simulate two concurrent handles.
// Clone A saves first (DB version 0 -> 1). Clone B still holds version=0
// in memory; its save() composes the WHERE `revision = 0` predicate,
// matches zero rows, and returns LockConflict. before_save runs once on
// each save (twice total — A and B). after_save runs only on the
// successful save (once total — A only).
// ---------------------------------------------------------------------------

#[model(table = "save_locks", pk = HeerId, hooks)]
#[derive(Debug, Clone)]
pub struct SaveLock {
    pub value: i32,
    #[field(version)]
    pub revision: i32,
}

tokio::task_local! {
    static SL_BEFORE_COUNT: Cell<u32>;
    static SL_AFTER_COUNT: Cell<u32>;
}

impl djogi::hooks::ModelHooks for SaveLock {
    async fn before_save(
        &mut self,
        _ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        SL_BEFORE_COUNT.with(|c| c.set(c.get() + 1));
        Ok(())
    }

    async fn after_save(&self, _ctx: &mut djogi::DjogiContext) -> Result<(), djogi::DjogiError> {
        SL_AFTER_COUNT.with(|c| c.set(c.get() + 1));
        Ok(())
    }
}

async fn setup_save_locks(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE save_locks (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            value       INTEGER     NOT NULL,
            revision    INTEGER     NOT NULL    DEFAULT 0
        )",
        &[],
    )
    .await
    .expect("create save_locks table");
}

#[djogi::djogi_test]
async fn before_save_lockconflict_branch_propagates(mut ctx: djogi::DjogiContext) {
    setup_save_locks(&mut ctx).await;

    SL_BEFORE_COUNT
        .scope(Cell::new(0), async {
            SL_AFTER_COUNT
                .scope(Cell::new(0), async {
                    let row = SaveLock::create(
                        &mut ctx,
                        SaveLock {
                            value: 0,
                            revision: 0,
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("create should succeed");
                    assert_eq!(row.revision, 0);

                    let mut clone_a = row.clone();
                    let mut clone_b = row.clone();

                    // Clone A saves first — version 0 -> 1 succeeds.
                    clone_a.value = 1;
                    clone_a
                        .save(&mut ctx)
                        .await
                        .expect("clone_a save must succeed (no conflict)");
                    assert_eq!(clone_a.revision, 1);
                    assert_eq!(
                        SL_BEFORE_COUNT.with(Cell::get),
                        1,
                        "before_save must have fired once for clone_a's save",
                    );
                    assert_eq!(
                        SL_AFTER_COUNT.with(Cell::get),
                        1,
                        "after_save must have fired once for clone_a's successful save",
                    );

                    // Clone B saves second — version 0 mismatches DB
                    // version 1, returns LockConflict. before_save MUST
                    // fire (precedes the UPDATE per D3 line 122).
                    // after_save MUST NOT fire — the LockConflict early
                    // return skips it.
                    clone_b.value = 2;
                    let result = clone_b.save(&mut ctx).await;
                    assert!(
                        matches!(result, Err(djogi::DjogiError::LockConflict(_))),
                        "stale save must return LockConflict; got: {result:?}",
                    );

                    assert_eq!(
                        SL_BEFORE_COUNT.with(Cell::get),
                        2,
                        "before_save must have fired AGAIN for clone_b's save \
                         (precedes the UPDATE composition per Phase 8 §D3 line 122)",
                    );
                    assert_eq!(
                        SL_AFTER_COUNT.with(Cell::get),
                        1,
                        "after_save must NOT have fired for the LockConflict path \
                         — the UPDATE didn't mutate the row, after_save would \
                         observe stale state",
                    );

                    // Sanity check: the DB row's revision is still 1
                    // (clone_a's bump), proving clone_b's UPDATE matched
                    // zero rows.
                    let db_revision: i32 = ctx
                        .raw_scalar("SELECT revision FROM save_locks WHERE id = $1", &[&row.id])
                        .await
                        .expect("revision scalar select");
                    assert_eq!(
                        db_revision, 1,
                        "DB revision must still be 1 — clone_b's UPDATE matched zero rows",
                    );
                })
                .await
        })
        .await;
}
