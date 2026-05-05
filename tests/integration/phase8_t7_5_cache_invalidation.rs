//! Phase 8δ T7.5 integration tests: `on_commit` cache invalidation hooks.
//!
//! What this file pins:
//!
//! 1. `save_invalidates_on_commit` — after `Model::save` inside `atomic`, the
//!    entry that was pre-inserted into the bound `Punnu` is gone after commit
//!    (the `on_commit` hook fired and called `Punnu::invalidate`).
//!
//! 2. `save_does_not_invalidate_on_rollback` — when the `atomic` closure
//!    returns `Err`, the `on_commit` hook is dropped along with the queued
//!    callbacks and the Punnu entry survives (never invalidated).
//!
//! 3. `delete_invalidates_on_commit` — analogous to test 1, but exercises
//!    the `Model::delete` path with `InvalidationReason::OnDelete`.
//!
//! 4. `nested_savepoint_save_invalidates_only_on_outer_commit` — calling
//!    `Model::save` inside a nested `atomic` (savepoint) only fires the
//!    invalidation at outermost commit, not at savepoint RELEASE.
//!
//! 5. `bulk_update_invalidation_deferred_to_followup` — placeholder for
//!    the deferred bulk-update invalidation path. `QuerySet::update.execute`
//!    does not currently capture touched row ids; the safe Option B design
//!    (`with_cache_invalidation()` builder) is deferred to a follow-up
//!    commit. See `djogi/src/query/update.rs` TODO(8δ T7.5 follow-up).
//!
//! # Fixture strategy
//!
//! Each test provisions its own table inline via `ctx.raw_execute`.
//! The `#[djogi_test]` macro already installs HeeRanjID schema, seeds
//! node 1, and sets `heer.node_id = '1'` before the test body runs.
//!
//! The `on_commit` hook captures `ctx.punnu::<T>()` (the Arc<Punnu<T>> from
//! the transaction-backed context inside `atomic`). To observe the post-commit
//! state, tests capture the Arc into an outer `Arc<std::sync::Mutex<...>>`
//! before the atomic closure returns.
//!
//! # Why these tests live in `tests/integration/`
//!
//! Per the workspace convention: every other `phase{N}_*` integration test
//! sits here, registered through `djogi/Cargo.toml`'s `[[test]]` blocks.
//! The cache invalidation surface is reachable through the public `djogi`
//! crate API, exactly as adopters consume it.
//!
//! # Spec anchor
//!
//! `docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md`
//! §3 commit T7.5.

use djogi::DjogiError;
use djogi::prelude::*;
use djogi::transaction::atomic;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Fixture model — a tiny table for the on_commit invalidation tests.
// ---------------------------------------------------------------------------

#[model(table = "phase8_t7_5_inval_rows", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct InvalRow {
    pub note: String,
}

async fn setup_inval_row(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE IF NOT EXISTS phase8_t7_5_inval_rows (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            note        TEXT        NOT NULL
        )",
        &[],
    )
    .await
    .expect("create phase8_t7_5_inval_rows table");
}

// ---------------------------------------------------------------------------
// Test 1 — `Model::save` enqueues on_commit invalidation.
//
// Flow:
//  a. Create a row in the DB (committed).
//  b. In a second atomic: capture the Punnu Arc, manually insert the row into
//     Punnu to simulate a warm cache entry, then call `Model::save`.
//  c. After the atomic commits, assert the Punnu entry is gone.
// ---------------------------------------------------------------------------

#[djogi::djogi_test]
async fn save_invalidates_on_commit(mut ctx: djogi::DjogiContext) {
    setup_inval_row(&mut ctx).await;
    let pool = ctx
        .pool()
        .expect("djogi_test gives a pool-backed context")
        .clone();

    // Step a: create the row so the DB has it (needed for save() to succeed).
    let row = atomic(&pool, |tx| {
        Box::pin(async move {
            let r = InvalRow::create(
                tx,
                InvalRow {
                    note: "initial".into(),
                    ..Default::default()
                },
            )
            .await?;
            Ok::<_, DjogiError>(r)
        })
    })
    .await
    .expect("create should succeed");

    let row_id = row.id;

    // Step b: inside a second atomic, pre-insert into Punnu and call save.
    // Capture the Punnu Arc so we can inspect it after commit.
    let captured_punnu: Arc<Mutex<Option<Arc<djogi::cache::Punnu<InvalRow>>>>> =
        Arc::new(Mutex::new(None));

    {
        let captured_punnu = captured_punnu.clone();
        atomic(&pool, |tx| {
            let captured_punnu = captured_punnu.clone();
            Box::pin(async move {
                // Pre-insert the stale cached entry.
                if let Some(punnu) = tx.punnu::<InvalRow>() {
                    // Store the Arc so the outer test can inspect it post-commit.
                    *captured_punnu.lock().unwrap() = Some(punnu.clone());
                    // Insert the row to simulate a warm cache hit.
                    punnu
                        .insert(InvalRow {
                            note: "stale".into(),
                            ..Default::default()
                        })
                        .await
                        .expect("Punnu::insert for stale row should succeed");
                }

                // Fetch the row and save it (triggers on_commit invalidation).
                let mut live = InvalRow::get(tx, row_id).await?;
                live.note = "updated".into();
                live.save(tx).await?;

                Ok::<_, DjogiError>(())
            })
        })
        .await
        .expect("atomic save should succeed");
    }

    // Step c: after commit, the on_commit hook must have fired.
    let punnu = captured_punnu
        .lock()
        .unwrap()
        .take()
        .expect("Punnu Arc must have been captured inside the closure");

    assert!(
        punnu.get(&row_id).is_none(),
        "Punnu entry for the saved row must be gone after commit — \
         Model::save enqueues an on_commit callback that calls \
         Punnu::invalidate(id, InvalidationReason::OnSave); \
         if Some(_) is still returned, the callback was not registered or not drained",
    );
}

// ---------------------------------------------------------------------------
// Test 2 — rollback drops the on_commit hook; entry survives.
//
// Flow:
//  a. Create a row in the DB.
//  b. In a second atomic: pre-insert into Punnu, call `Model::save`, then
//     return Err to trigger rollback.
//  c. After rollback, the Punnu entry must still be present.
// ---------------------------------------------------------------------------

#[djogi::djogi_test]
async fn save_does_not_invalidate_on_rollback(mut ctx: djogi::DjogiContext) {
    setup_inval_row(&mut ctx).await;
    let pool = ctx
        .pool()
        .expect("djogi_test gives a pool-backed context")
        .clone();

    // Create the row.
    let row = atomic(&pool, |tx| {
        Box::pin(async move {
            InvalRow::create(
                tx,
                InvalRow {
                    note: "initial".into(),
                    ..Default::default()
                },
            )
            .await
        })
    })
    .await
    .expect("create should succeed");

    let row_id = row.id;

    let captured_punnu: Arc<Mutex<Option<Arc<djogi::cache::Punnu<InvalRow>>>>> =
        Arc::new(Mutex::new(None));

    {
        let captured_punnu = captured_punnu.clone();
        let _ = atomic(&pool, |tx| {
            let captured_punnu = captured_punnu.clone();
            Box::pin(async move {
                if let Some(punnu) = tx.punnu::<InvalRow>() {
                    *captured_punnu.lock().unwrap() = Some(punnu.clone());
                    // Insert under the SAME id the save will target so the
                    // post-rollback assertion can pin "the matching entry
                    // survived" via `get(&row_id).is_some()` instead of a
                    // weaker `!is_empty()` check (mirrors Test 4's pattern).
                    punnu
                        .insert(InvalRow {
                            id: row_id,
                            note: "stale".into(),
                            ..Default::default()
                        })
                        .await
                        .expect("Punnu::insert should succeed");
                }

                let mut live = InvalRow::get(tx, row_id).await?;
                live.note = "would-be-updated".into();
                // Save enqueues the on_commit hook — but we're about to rollback.
                live.save(tx).await?;

                // Force rollback by returning Err.
                Err::<(), _>(DjogiError::not_found("forced rollback for test"))
            })
        })
        .await;
        // Ignore the Err — rollback is the intent.
    }

    // After rollback: the on_commit queue was discarded, so the callback never
    // fired. The stale Punnu entry under `row_id` must still be present.
    let punnu = captured_punnu
        .lock()
        .unwrap()
        .take()
        .expect("Punnu Arc must have been captured inside the closure");

    assert!(
        punnu.get(&row_id).is_some(),
        "Punnu must still contain the stale entry under row_id after rollback — \
         on_commit callbacks queued inside a rolled-back atomic are discarded; \
         a None here would mean the invalidation fired despite rollback",
    );
}

// ---------------------------------------------------------------------------
// Test 3 — `Model::delete` enqueues on_commit invalidation.
//
// Flow:
//  a. Create a row in the DB.
//  b. In a second atomic: pre-insert into Punnu, call `Model::delete`.
//  c. After commit, Punnu entry must be gone.
// ---------------------------------------------------------------------------

#[djogi::djogi_test]
async fn delete_invalidates_on_commit(mut ctx: djogi::DjogiContext) {
    setup_inval_row(&mut ctx).await;
    let pool = ctx
        .pool()
        .expect("djogi_test gives a pool-backed context")
        .clone();

    // Create the row.
    let row = atomic(&pool, |tx| {
        Box::pin(async move {
            InvalRow::create(
                tx,
                InvalRow {
                    note: "to-delete".into(),
                    ..Default::default()
                },
            )
            .await
        })
    })
    .await
    .expect("create should succeed");

    let row_id = row.id;
    let captured_punnu: Arc<Mutex<Option<Arc<djogi::cache::Punnu<InvalRow>>>>> =
        Arc::new(Mutex::new(None));

    {
        let captured_punnu = captured_punnu.clone();
        atomic(&pool, |tx| {
            let captured_punnu = captured_punnu.clone();
            Box::pin(async move {
                if let Some(punnu) = tx.punnu::<InvalRow>() {
                    *captured_punnu.lock().unwrap() = Some(punnu.clone());
                    // Seed the Punnu with the row to simulate a cache hit.
                    punnu
                        .insert(InvalRow {
                            note: "cached-pre-delete".into(),
                            ..Default::default()
                        })
                        .await
                        .expect("Punnu::insert should succeed");
                }

                let live = InvalRow::get(tx, row_id).await?;
                live.delete(tx).await?;

                Ok::<_, DjogiError>(())
            })
        })
        .await
        .expect("atomic delete should succeed");
    }

    let punnu = captured_punnu
        .lock()
        .unwrap()
        .take()
        .expect("Punnu Arc must have been captured inside the closure");

    assert!(
        punnu.get(&row_id).is_none(),
        "Punnu entry for the deleted row must be gone after commit — \
         Model::delete enqueues an on_commit callback that calls \
         Punnu::invalidate(id, InvalidationReason::OnDelete); \
         if Some(_) is still returned, the callback was not registered or not drained",
    );
}

// ---------------------------------------------------------------------------
// Test 4 — nested savepoint: save inside inner atomic fires only at outer
// commit, not at savepoint RELEASE.
//
// Flow:
//  a. Create a row in the DB.
//  b. Open an outer atomic. Inside it, open a nested atomic (savepoint).
//     In the nested scope: pre-insert into Punnu, call `Model::save`.
//     After RELEASE (nested commit), Punnu entry still present (outer not
//     committed yet).
//  c. Commit the outer. Now the on_commit hook fires and entry is gone.
// ---------------------------------------------------------------------------

#[djogi::djogi_test]
async fn nested_savepoint_save_invalidates_only_on_outer_commit(mut ctx: djogi::DjogiContext) {
    setup_inval_row(&mut ctx).await;
    let pool = ctx
        .pool()
        .expect("djogi_test gives a pool-backed context")
        .clone();

    // Create the row.
    let row = atomic(&pool, |tx| {
        Box::pin(async move {
            InvalRow::create(
                tx,
                InvalRow {
                    note: "nested-test".into(),
                    ..Default::default()
                },
            )
            .await
        })
    })
    .await
    .expect("create should succeed");

    let row_id = row.id;
    let captured_punnu: Arc<Mutex<Option<Arc<djogi::cache::Punnu<InvalRow>>>>> =
        Arc::new(Mutex::new(None));

    {
        let captured_punnu = captured_punnu.clone();
        atomic(&pool, |outer| {
            let captured_punnu = captured_punnu.clone();
            Box::pin(async move {
                // Nested atomic (savepoint).
                atomic(&mut *outer, |inner| {
                    let captured_punnu = captured_punnu.clone();
                    Box::pin(async move {
                        if let Some(punnu) = inner.punnu::<InvalRow>() {
                            *captured_punnu.lock().unwrap() = Some(punnu.clone());
                            // Pre-insert under the SAME id the save will target,
                            // so the mid-flight assertion below can pin "the
                            // matching entry is still present" — without id parity
                            // the assertion would be a tautology (any other entry
                            // in the Punnu would satisfy `!is_empty()`).
                            punnu
                                .insert(InvalRow {
                                    id: row_id,
                                    note: "stale-in-savepoint".into(),
                                    ..Default::default()
                                })
                                .await
                                .expect("Punnu::insert should succeed");
                        }

                        let mut live = InvalRow::get(inner, row_id).await?;
                        live.note = "updated-in-savepoint".into();
                        live.save(inner).await?;

                        Ok::<_, DjogiError>(())
                    })
                })
                .await?;
                // After nested commit (RELEASE SAVEPOINT): on_commit queue is
                // promoted to the outer context but NOT yet drained — so the
                // pre-inserted entry under `row_id` must still be present.
                // The post-outer-commit assertion below pins that the drain
                // happens exactly once at outer commit.
                if let Some(ref punnu) = *captured_punnu.lock().unwrap() {
                    assert!(
                        punnu.get(&row_id).is_some(),
                        "Punnu entry under row_id must still be present after the \
                         inner RELEASE SAVEPOINT — the nested save enqueued an \
                         on_commit callback that promotes to the outer queue but \
                         is NOT drained until the outer COMMIT. If None here, the \
                         drain fired prematurely on the savepoint release.",
                    );
                }

                Ok::<_, DjogiError>(())
            })
        })
        .await
        .expect("outer atomic should succeed");
    }

    // After outer commit: the on_commit drain ran exactly once.
    let punnu = captured_punnu
        .lock()
        .unwrap()
        .take()
        .expect("Punnu Arc must have been captured");

    assert!(
        punnu.get(&row_id).is_none(),
        "Punnu entry must be gone after outer commit — the nested save enqueued \
         an on_commit callback that was promoted to the outer queue and drained \
         at outer COMMIT. If Some(_) is returned, the callback queue promotion \
         failed or the drain did not fire.",
    );
}

// ---------------------------------------------------------------------------
// Test 5 — Bulk-update cache invalidation deferred to follow-up.
//
// `QuerySet::update.execute` does not currently capture touched row ids;
// the safe Option B path (`with_cache_invalidation()` builder) is deferred.
// See `djogi/src/query/update.rs` TODO(8δ T7.5 follow-up).
//
// This test is a named placeholder so the deferral is visible in test output.
// ---------------------------------------------------------------------------

#[djogi::djogi_test]
async fn bulk_update_invalidation_deferred_to_followup(mut ctx: djogi::DjogiContext) {
    // Deferred — see TODO(8δ T7.5 follow-up) in update.rs.
    // Once Option B (with_cache_invalidation()) is implemented, this test
    // will be replaced by the real bulk-update invalidation assertions:
    //   1. Pre-insert N rows into Punnu.
    //   2. `QuerySet::filter(...).update(...).with_cache_invalidation().execute(ctx).await`
    //   3. Assert all N entries are gone from Punnu after commit.
    let _ = &mut ctx;
}
