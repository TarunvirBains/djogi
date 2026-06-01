// T7.5 integration tests: `on_commit` cache invalidation hooks.
//
// What this file pins:
//
// 1. `save_invalidates_on_commit` — after `Model::save` inside `atomic`, the
//    entry that was pre-inserted into the bound `Punnu` is gone after commit
//    (the `on_commit` hook fired and called `Punnu::invalidate`).
//
// 2. `save_does_not_invalidate_on_rollback` — when the `atomic` closure
//    returns `Err`, the `on_commit` hook is dropped along with the queued
//    callbacks and the Punnu entry survives (never invalidated).
//
// 3. `delete_invalidates_on_commit` — analogous to test 1, but exercises
//    the `Model::delete` path with `InvalidationReason::OnDelete`.
//
// 4. `nested_savepoint_save_invalidates_only_on_outer_commit` — calling
//    `Model::save` inside a nested `atomic` (savepoint) only fires the
//    invalidation at outermost commit, not at savepoint RELEASE.
//
// # Fixture strategy
//
// Tables are provisioned via `#[djogi_test(sync_models = [InvalRow])]`
// which routes through the same migration engine that production uses.
// The `#[djogi_test]` macro already installs HeeRanjID schema, seeds
// node 1, and sets `heer.node_id = '1'` before the test body runs.
//
// The `on_commit` hook captures `ctx.punnu::<T>()` (the Arc<Punnu<T>> from
// the transaction-backed context inside `atomic`). To observe the post-commit
// state, tests capture the Arc into an outer `Arc<std::sync::Mutex<...>>`
// before the atomic closure returns.
//
// # Why these tests live in `tests/integration/`
//
// Per the workspace convention: every other `phase{N}_*` integration test
// sits here, registered through `djogi/Cargo.toml`'s `[[test]]` blocks.
// The cache invalidation surface is reachable through the public `djogi`
// crate API, exactly as adopters consume it.
//
// # Spec anchor
//
// `docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md`
// §3 commit T7.5.

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

// ---------------------------------------------------------------------------
// Test 1 — `Model::save` enqueues on_commit invalidation.
//
// Flow:
//  a. Create a row in the DB (committed).
//  b. In a second atomic: capture the Punnu Arc, manually insert the row into
//     Punnu to simulate a warm cache entry, then call `Model::save`.
//  c. After the atomic commits, assert the Punnu entry is gone.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [InvalRow])]
async fn save_invalidates_on_commit(ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must be pool-backed");

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
                    // Insert under the SAME id the save will target so the
                    // post-commit assertion is a real check ("entry seeded
                    // under row_id → save fired → on_commit drained →
                    // invalidation removed it"). Without id parity the
                    // assertion is a tautology: get(&row_id) would be None
                    // regardless of whether the hook fired.
                    punnu
                        .insert(InvalRow {
                            id: row_id,
                            note: "stale-pre-save".into(),
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

#[djogi::djogi_test(sync_models = [InvalRow])]
async fn save_does_not_invalidate_on_rollback(ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must be pool-backed");

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

#[djogi::djogi_test(sync_models = [InvalRow])]
async fn delete_invalidates_on_commit(ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must be pool-backed");

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
                    // Seed the Punnu under the SAME id the delete will
                    // target so the post-commit assertion is a real check
                    // ("entry seeded under row_id → delete fired →
                    // on_commit drained → invalidation removed it").
                    // Without id parity the assertion would be a tautology.
                    punnu
                        .insert(InvalRow {
                            id: row_id,
                            note: "stale-pre-delete".into(),
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

#[djogi::djogi_test(sync_models = [InvalRow])]
async fn nested_savepoint_save_invalidates_only_on_outer_commit(ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must be pool-backed");

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
// Test 5 — bulk `execute_returning_pairs` enqueues per-row OnSave invalidation
// and drains on commit.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [InvalRow])]
async fn bulk_execute_returning_pairs_invalidates_on_commit(ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must be pool-backed");

    let created_rows = atomic(&pool, |tx| {
        Box::pin(async move {
            let mut rows = Vec::new();
            for i in 0..3 {
                rows.push(
                    InvalRow::create(
                        tx,
                        InvalRow {
                            note: format!("bulk-{i}"),
                            ..Default::default()
                        },
                    )
                    .await?,
                );
            }
            Ok::<_, DjogiError>(rows)
        })
    })
    .await
    .expect("bulk seed should succeed");

    let row_ids: Vec<_> = created_rows.iter().map(|row| row.id).collect();
    let captured_punnu: Arc<Mutex<Option<Arc<djogi::cache::Punnu<InvalRow>>>>> =
        Arc::new(Mutex::new(None));

    {
        let captured_punnu = captured_punnu.clone();
        let row_ids = row_ids.clone();
        atomic(&pool, |tx| {
            let captured_punnu = captured_punnu.clone();
            let row_ids = row_ids.clone();
            Box::pin(async move {
                if let Some(punnu) = tx.punnu::<InvalRow>() {
                    *captured_punnu.lock().unwrap() = Some(punnu.clone());
                    for id in &row_ids {
                        punnu
                            .insert(InvalRow {
                                id: *id,
                                note: "stale-pre-bulk-update".into(),
                                ..Default::default()
                            })
                            .await
                            .expect("Punnu::insert should succeed");
                    }
                }

                let pairs = InvalRow::objects()
                    .update(|f| f.note().set("bulk-updated".to_string()))
                    .execute_returning_pairs(tx)
                    .await?;
                assert_eq!(
                    pairs.len(),
                    row_ids.len(),
                    "bulk execute_returning_pairs should update every seeded row"
                );

                Ok::<_, DjogiError>(())
            })
        })
        .await
        .expect("bulk execute_returning_pairs should succeed");
    }

    let punnu = captured_punnu
        .lock()
        .unwrap()
        .take()
        .expect("Punnu Arc must have been captured inside the closure");

    for id in row_ids {
        assert!(
            punnu.get(&id).is_none(),
            "Punnu entry for each bulk-updated row must be gone after commit; \
             execute_returning_pairs enqueues per-row OnSave invalidation via on_commit"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 6 — plain bulk `execute` enqueues on_commit invalidation for warmed
// Punnu rows and drains on commit.
//
// REQ-304-2: Bulk update via `.execute()` in a transaction-backed context
// should collect affected IDs using `UPDATE ... RETURNING id` and enqueue
// one bulk `on_commit` invalidation callback. After the atomic commits,
// all warmed Punnu entries for the affected rows must be gone.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [InvalRow])]
async fn bulk_update_execute_invalidates_on_commit(ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must be pool-backed");

    // Seed rows.
    let created_rows = atomic(&pool, |tx| {
        Box::pin(async move {
            let mut rows = Vec::new();
            for i in 0..3 {
                rows.push(
                    InvalRow::create(
                        tx,
                        InvalRow {
                            note: format!("bulk-execute-{i}"),
                            ..Default::default()
                        },
                    )
                    .await?,
                );
            }
            Ok::<_, DjogiError>(rows)
        })
    })
    .await
    .expect("bulk seed should succeed");

    let row_ids: Vec<_> = created_rows.iter().map(|row| row.id).collect();
    let captured_punnu: Arc<Mutex<Option<Arc<djogi::cache::Punnu<InvalRow>>>>> =
        Arc::new(Mutex::new(None));

    {
        let captured_punnu = captured_punnu.clone();
        let row_ids = row_ids.clone();
        atomic(&pool, |tx| {
            let captured_punnu = captured_punnu.clone();
            let row_ids = row_ids.clone();
            Box::pin(async move {
                // Pre-insert stale cached entries for each row.
                if let Some(punnu) = tx.punnu::<InvalRow>() {
                    *captured_punnu.lock().unwrap() = Some(punnu.clone());
                    for id in &row_ids {
                        punnu
                            .insert(InvalRow {
                                id: *id,
                                note: "stale-pre-bulk-execute".into(),
                                ..Default::default()
                            })
                            .await
                            .expect("Punnu::insert should succeed");
                    }
                }

                // Bulk update via plain `.execute()` — no returning pairs.
                let affected = InvalRow::objects()
                    .update(|f| f.note().set("bulk-execute-updated".to_string()))
                    .execute(tx)
                    .await?;
                assert_eq!(
                    affected,
                    row_ids.len() as u64,
                    "plain bulk execute should update every seeded row"
                );

                Ok::<_, DjogiError>(())
            })
        })
        .await
        .expect("bulk execute should succeed");
    }

    // After commit: all warmed Punnu entries must be invalidated.
    let punnu = captured_punnu
        .lock()
        .unwrap()
        .take()
        .expect("Punnu Arc must have been captured inside the closure");

    for id in row_ids {
        assert!(
            punnu.get(&id).is_none(),
            "Punnu entry for each bulk-updated row must be gone after commit; \
             plain execute in a transaction-backed context collects affected IDs \
             via UPDATE ... RETURNING id and enqueues one on_commit invalidation"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 7 — rollback drops the on_commit hook from plain bulk `execute`;
// warmed Punnu entries survive.
//
// REQ-304-3: When a transaction containing a plain bulk `.execute()` is
// rolled back, the on_commit callbacks queued inside are discarded and
// the Punnu entries remain cached (never invalidated).
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [InvalRow])]
async fn bulk_update_execute_does_not_invalidate_on_rollback(ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must be pool-backed");

    // Seed rows.
    let created_rows = atomic(&pool, |tx| {
        Box::pin(async move {
            let mut rows = Vec::new();
            for i in 0..3 {
                rows.push(
                    InvalRow::create(
                        tx,
                        InvalRow {
                            note: format!("rollback-{i}"),
                            ..Default::default()
                        },
                    )
                    .await?,
                );
            }
            Ok::<_, DjogiError>(rows)
        })
    })
    .await
    .expect("bulk seed should succeed");

    let row_ids: Vec<_> = created_rows.iter().map(|row| row.id).collect();
    let captured_punnu: Arc<Mutex<Option<Arc<djogi::cache::Punnu<InvalRow>>>>> =
        Arc::new(Mutex::new(None));

    {
        let captured_punnu = captured_punnu.clone();
        let row_ids = row_ids.clone();
        let _ = atomic(&pool, |tx| {
            let captured_punnu = captured_punnu.clone();
            Box::pin(async move {
                // Pre-insert stale cached entries.
                if let Some(punnu) = tx.punnu::<InvalRow>() {
                    *captured_punnu.lock().unwrap() = Some(punnu.clone());
                    for id in &row_ids {
                        punnu
                            .insert(InvalRow {
                                id: *id,
                                note: "stale".into(),
                                ..Default::default()
                            })
                            .await
                            .expect("Punnu::insert should succeed");
                    }
                }

                // Bulk update via plain `.execute()`.
                InvalRow::objects()
                    .update(|f| f.note().set("rolled-back".to_string()))
                    .execute(tx)
                    .await?;

                // Force rollback.
                Err::<(), _>(DjogiError::not_found("forced rollback for test"))
            })
        })
        .await;
        // Ignore the Err — rollback is the intent.
    }

    // After rollback: on_commit queue discarded, Punnu entries survive.
    let punnu = captured_punnu
        .lock()
        .unwrap()
        .take()
        .expect("Punnu Arc must have been captured inside the closure");

    for id in row_ids {
        assert!(
            punnu.get(&id).is_some(),
            "Punnu entry under row_id must still be present after rollback — \
             on_commit callbacks queued inside a rolled-back atomic are discarded; \
             a None here would mean the invalidation fired despite rollback"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 8 — rollback drops the on_commit hook from `execute_returning_pairs`;
// warmed Punnu entries survive.
//
// REQ-304-4: When a transaction containing bulk `.execute_returning_pairs()`
// is rolled back, the per-row on_commit callbacks queued inside are discarded
// and the Punnu entries remain cached (never invalidated).
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [InvalRow])]
async fn bulk_execute_returning_pairs_does_not_invalidate_on_rollback(ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must be pool-backed");

    // Seed rows.
    let created_rows = atomic(&pool, |tx| {
        Box::pin(async move {
            let mut rows = Vec::new();
            for i in 0..3 {
                rows.push(
                    InvalRow::create(
                        tx,
                        InvalRow {
                            note: format!("returning-pairs-rb-{i}"),
                            ..Default::default()
                        },
                    )
                    .await?,
                );
            }
            Ok::<_, DjogiError>(rows)
        })
    })
    .await
    .expect("bulk seed should succeed");

    let row_ids: Vec<_> = created_rows.iter().map(|row| row.id).collect();
    let captured_punnu: Arc<Mutex<Option<Arc<djogi::cache::Punnu<InvalRow>>>>> =
        Arc::new(Mutex::new(None));

    {
        let captured_punnu = captured_punnu.clone();
        let row_ids = row_ids.clone();
        let _ = atomic(&pool, |tx| {
            let captured_punnu = captured_punnu.clone();
            Box::pin(async move {
                // Pre-insert stale cached entries.
                if let Some(punnu) = tx.punnu::<InvalRow>() {
                    *captured_punnu.lock().unwrap() = Some(punnu.clone());
                    for id in &row_ids {
                        punnu
                            .insert(InvalRow {
                                id: *id,
                                note: "stale".into(),
                                ..Default::default()
                            })
                            .await
                            .expect("Punnu::insert should succeed");
                    }
                }

                // Bulk update via returning pairs.
                let _pairs = InvalRow::objects()
                    .update(|f| f.note().set("rolled-back-returning".to_string()))
                    .execute_returning_pairs(tx)
                    .await?;

                // Force rollback.
                Err::<(), _>(DjogiError::not_found("forced rollback for test"))
            })
        })
        .await;
        // Ignore the Err — rollback is the intent.
    }

    // After rollback: on_commit queue discarded, Punnu entries survive.
    let punnu = captured_punnu
        .lock()
        .unwrap()
        .take()
        .expect("Punnu Arc must have been captured inside the closure");

    for id in row_ids {
        assert!(
            punnu.get(&id).is_some(),
            "Punnu entry under row_id must still be present after rollback — \
             execute_returning_pairs enqueues per-row OnSave invalidation via \
             on_commit, but a rolled-back atomic discards those callbacks; \
             a None here would mean the invalidation fired despite rollback"
        );
    }
}
