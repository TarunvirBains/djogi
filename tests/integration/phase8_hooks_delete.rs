//! Phase 8α T1.6 integration tests: `before_delete` + `after_delete`
//! dispatch around the macro-emitted `Model::delete()` body.
//!
//! What this file pins:
//!
//! 1. `before_delete(&mut self, ctx)` fires before the DELETE composes —
//!    while the row is still queryable from the database. The hook can
//!    inspect (and mutate) the in-memory snapshot before it is consumed.
//! 2. `after_delete(&self, ctx)` fires AFTER the DELETE statement runs
//!    AND after the outbox row is emitted — the hook can read the
//!    canonical pre-delete payload via the outbox while observing that
//!    the primary row is gone.
//! 3. Returning `Err` from `before_delete` short-circuits the entire
//!    sequence: no DELETE composes, no outbox row is written. Wrapped in
//!    `atomic()`, the surrounding transaction rolls back via standard
//!    `?` propagation; a follow-up re-fetch confirms the row still
//!    exists unchanged.
//! 4. Even though `delete(self, ctx)` consumes `self`, `after_delete`
//!    still sees mutations the `before_delete` body made to `self` —
//!    the macro re-binds `self` as `mut self` inside the body so both
//!    hooks share the same in-memory value.
//!
//! Phase 8 §D3 lines 118-129 fix the canonical sequence as
//! `before_delete -> DELETE -> outbox -> after_delete -> on_commit drain`.
//! Order is load-bearing: Test 2 here pins the after_delete-sees-outbox
//! invariant directly.
//!
//! # One model per test — coherence
//!
//! `impl ModelHooks for T` is a coherent impl: only one per `T` per
//! crate. Each test therefore declares its own model type sharing a
//! `hook_delete_*` table shape. Test 2 is the events-model variant
//! (uses `#[model(events)]` + companion `_outbox` table).
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
// Test 1 — before_delete fires while the row is still in the DB.
//
// The hook body issues a raw SELECT for the row's `id` and confirms a
// row is returned (count == 1). After delete() returns, the test body
// confirms the count is now 0 — proving the hook saw the pre-DELETE
// state, not the post-DELETE state.
// ---------------------------------------------------------------------------

#[model(table = "del_pre_witness", pk = HeerId, hooks)]
#[derive(Debug, Clone)]
pub struct DelPreWitness {
    pub value: i32,
}

tokio::task_local! {
    static DPW_BEFORE_SAW_ROW: Cell<bool>;
}

impl djogi::hooks::ModelHooks for DelPreWitness {
    async fn before_delete(
        &mut self,
        ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        // The DELETE has not run yet — we MUST see exactly one row.
        let n: i64 = ctx
            .raw_scalar(
                "SELECT COUNT(*) FROM del_pre_witness WHERE id = $1",
                &[&self.id],
            )
            .await?;
        DPW_BEFORE_SAW_ROW.with(|c| c.set(n == 1));
        Ok(())
    }
}

async fn setup_del_pre_witness(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE del_pre_witness (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            value       INTEGER     NOT NULL
        )",
        &[],
    )
    .await
    .expect("create del_pre_witness table");
}

#[djogi::djogi_test]
async fn before_delete_fires_pre_db_delete(mut ctx: djogi::DjogiContext) {
    setup_del_pre_witness(&mut ctx).await;

    DPW_BEFORE_SAW_ROW
        .scope(Cell::new(false), async {
            let row = DelPreWitness::create(
                &mut ctx,
                DelPreWitness {
                    value: 7,
                    ..Default::default()
                },
            )
            .await
            .expect("create should succeed");
            let row_id = row.id;

            row.delete(&mut ctx)
                .await
                .expect("delete should succeed and run before_delete");

            assert!(
                DPW_BEFORE_SAW_ROW.with(Cell::get),
                "before_delete must fire BEFORE the DELETE composes — \
                 the SELECT inside the hook body must observe the row",
            );

            // Sanity: after delete() returns, the row is gone.
            let n: i64 = ctx
                .raw_scalar(
                    "SELECT COUNT(*) FROM del_pre_witness WHERE id = $1",
                    &[&row_id],
                )
                .await
                .expect("post-delete count");
            assert_eq!(n, 0, "primary row must be gone after delete() returns",);
        })
        .await;
}

// ---------------------------------------------------------------------------
// Test 2 — after_delete runs AFTER the outbox emission.
//
// Uses `#[model(events)]` + a hand-rolled `<table>_outbox` companion
// table (the same pattern Phase 4's tests follow until macro-side DDL
// emission lands). Inside `after_delete`, query the outbox row by
// `row_id` and assert (a) exactly one row exists, (b) `action='delete'`.
// This pins the §D3 invariant that an audit sink consuming the outbox
// sees the row before after_delete's body runs.
// ---------------------------------------------------------------------------

#[model(table = "del_outbox_witness", pk = HeerId, events, hooks)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct DelOutboxWitness {
    pub kind: String,
}

tokio::task_local! {
    static DOW_AFTER_SAW_OUTBOX: Cell<bool>;
    static DOW_AFTER_OUTBOX_ACTION_OK: Cell<bool>;
}

impl djogi::hooks::ModelHooks for DelOutboxWitness {
    async fn after_delete(&self, ctx: &mut djogi::DjogiContext) -> Result<(), djogi::DjogiError> {
        // Outbox row should already be in place by the time after_delete
        // fires (Phase 8 §D3 step 3 precedes step 4). Use raw_rows so we
        // can probe presence + the action column without requiring a
        // FromPgRow decoder for an ad-hoc shape.
        let rows = ctx
            .raw_rows(
                "SELECT action FROM del_outbox_witness_outbox \
                 WHERE row_id = $1",
                &[&self.id],
            )
            .await?;
        if let Some(r) = rows.first() {
            DOW_AFTER_SAW_OUTBOX.with(|c| c.set(true));
            let action: String = r.try_get("action").unwrap_or_default();
            DOW_AFTER_OUTBOX_ACTION_OK.with(|c| c.set(action == "delete"));
        } else {
            DOW_AFTER_SAW_OUTBOX.with(|c| c.set(false));
        }
        Ok(())
    }
}

async fn setup_del_outbox_witness(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE del_outbox_witness (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            kind        TEXT        NOT NULL
        )",
        &[],
    )
    .await
    .expect("create del_outbox_witness table");
    ctx.raw_execute(
        "CREATE TABLE del_outbox_witness_outbox (
            id         BIGINT      PRIMARY KEY DEFAULT generate_id(),
            row_id     BIGINT      NOT NULL,
            action     TEXT        NOT NULL,
            payload    JSONB       NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
        &[],
    )
    .await
    .expect("create del_outbox_witness_outbox table");
}

#[djogi::djogi_test]
async fn after_delete_runs_post_outbox(mut ctx: djogi::DjogiContext) {
    setup_del_outbox_witness(&mut ctx).await;

    DOW_AFTER_SAW_OUTBOX
        .scope(Cell::new(false), async {
            DOW_AFTER_OUTBOX_ACTION_OK
                .scope(Cell::new(false), async {
                    let row = DelOutboxWitness::create(
                        &mut ctx,
                        DelOutboxWitness {
                            kind: "goodbye".to_string(),
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("create should succeed");

                    // Clear the outbox so the after_delete check sees only
                    // the row written by the DELETE path.
                    ctx.raw_execute("DELETE FROM del_outbox_witness_outbox", &[])
                        .await
                        .expect("clear outbox");

                    row.delete(&mut ctx)
                        .await
                        .expect("delete should succeed and run after_delete");

                    assert!(
                        DOW_AFTER_SAW_OUTBOX.with(Cell::get),
                        "after_delete must observe the outbox row — \
                         outbox emission (D3 step 3) precedes after_delete \
                         (D3 step 4)",
                    );
                    assert!(
                        DOW_AFTER_OUTBOX_ACTION_OK.with(Cell::get),
                        "outbox row must record action='delete'",
                    );
                })
                .await
        })
        .await;
}

// ---------------------------------------------------------------------------
// Test 3 — before_delete returning Err aborts the DELETE.
//
// Wrapped in atomic() so the rollback is observable: the row must
// remain present after the failed delete().
// ---------------------------------------------------------------------------

#[model(table = "del_aborts", pk = HeerId, hooks)]
#[derive(Debug, Clone)]
pub struct DelAbort {
    pub value: i32,
}

impl djogi::hooks::ModelHooks for DelAbort {
    async fn before_delete(
        &mut self,
        _ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        Err(djogi::DjogiError::Validation("nope-delete".into()))
    }
}

async fn setup_del_aborts(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE del_aborts (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            value       INTEGER     NOT NULL
        )",
        &[],
    )
    .await
    .expect("create del_aborts table");
}

#[djogi::djogi_test]
async fn before_delete_err_aborts(mut ctx: djogi::DjogiContext) {
    setup_del_aborts(&mut ctx).await;
    let pool = ctx.pool().expect("djogi_test ctx is pool-backed").clone();

    // Insert a row OUTSIDE the atomic() so it survives when the inner
    // transaction rolls back.
    let row = DelAbort::create(
        &mut ctx,
        DelAbort {
            value: 1,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");
    let row_id = row.id;

    // Attempt to delete the row inside an atomic(). before_delete returns
    // Err — the DELETE never composes, the outer atomic() rolls back via
    // `?` propagation.
    let res: Result<(), djogi::DjogiError> = atomic(&pool, |ctx| {
        Box::pin(async move {
            let loaded = DelAbort::get(ctx, row_id).await?;
            loaded.delete(ctx).await?;
            // Should never reach here — before_delete's Err short-circuits.
            Ok(())
        })
    })
    .await;

    let Err(djogi::DjogiError::Validation(msg)) = res else {
        panic!("expected Err(DjogiError::Validation(_)), got {res:?}");
    };
    assert_eq!(
        msg, "nope-delete",
        "before_delete's Err variant must propagate unchanged",
    );

    // Re-fetch the row outside the rolled-back atomic. The DB row must
    // still exist: before_delete aborted before the DELETE composed, so
    // even without the atomic() rollback there was no DELETE to undo.
    let n: i64 = ctx
        .raw_scalar("SELECT COUNT(*) FROM del_aborts WHERE id = $1", &[&row_id])
        .await
        .expect("re-fetch should succeed");
    assert_eq!(
        n, 1,
        "before_delete returning Err must leave the DB row in place",
    );
}

// ---------------------------------------------------------------------------
// Test 4 — `delete(self, ctx)` consumes self, but the macro re-binds it
// as `mut self` so before_delete (`&mut self`) and after_delete
// (`&self`) share the same in-memory value. Mutations the before-hook
// makes are visible inside the after-hook even though the DB row is
// gone — proving the consumed-self shim wires the two hooks correctly.
// ---------------------------------------------------------------------------

#[model(table = "del_self_inspect", pk = HeerId, hooks)]
#[derive(Debug, Clone)]
pub struct DelSelfInspect {
    pub flag: bool,
    pub note: String,
}

tokio::task_local! {
    static DSI_AFTER_SAW_FLAG: Cell<bool>;
    static DSI_AFTER_SAW_NOTE_LEN: Cell<usize>;
}

impl djogi::hooks::ModelHooks for DelSelfInspect {
    async fn before_delete(
        &mut self,
        _ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        // Mutate in-memory state. The DB row is about to be deleted —
        // these writes never reach the DB, but after_delete must still
        // observe them.
        self.flag = true;
        self.note = "set-by-before-delete".to_string();
        Ok(())
    }

    async fn after_delete(&self, _ctx: &mut djogi::DjogiContext) -> Result<(), djogi::DjogiError> {
        DSI_AFTER_SAW_FLAG.with(|c| c.set(self.flag));
        DSI_AFTER_SAW_NOTE_LEN.with(|c| c.set(self.note.len()));
        Ok(())
    }
}

async fn setup_del_self_inspect(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE del_self_inspect (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            flag        BOOLEAN     NOT NULL    DEFAULT false,
            note        TEXT        NOT NULL    DEFAULT ''
        )",
        &[],
    )
    .await
    .expect("create del_self_inspect table");
}

#[djogi::djogi_test]
async fn delete_consumes_self_hook_can_inspect(mut ctx: djogi::DjogiContext) {
    setup_del_self_inspect(&mut ctx).await;

    DSI_AFTER_SAW_FLAG
        .scope(Cell::new(false), async {
            DSI_AFTER_SAW_NOTE_LEN
                .scope(Cell::new(0), async {
                    let row = DelSelfInspect::create(
                        &mut ctx,
                        DelSelfInspect {
                            flag: false,
                            note: String::new(),
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("create should succeed");

                    row.delete(&mut ctx)
                        .await
                        .expect("delete should succeed and run both hooks");

                    assert!(
                        DSI_AFTER_SAW_FLAG.with(Cell::get),
                        "after_delete must observe before_delete's mutation \
                         to `self.flag` even though the DB row is gone — \
                         proves the macro re-binds `self` as `mut self` so \
                         both hooks share the same in-memory value",
                    );
                    assert_eq!(
                        DSI_AFTER_SAW_NOTE_LEN.with(Cell::get),
                        "set-by-before-delete".len(),
                        "after_delete must observe before_delete's mutation \
                         to `self.note`",
                    );
                })
                .await
        })
        .await;
}
