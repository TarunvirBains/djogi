// Phase 4 Task 1 integration tests: `atomic()` + savepoints + on_commit
// drain + transaction-backed prefetch against live Postgres.
//
// What this file pins:
//
// 1. `run_atomic(&mut ctx, |ctx| Box::pin(async move { ... }))` opens a
//    transaction, runs the closure, and commits on `Ok`. Rows written
//    inside are visible after the scope returns.
// 2. Returning `Err` from the closure rolls the transaction back — no
//    rows survive.
// 3. Nested `atomic(&mut *ctx, ...)` emits `SAVEPOINT sp_<depth>`. An
//    inner rollback leaves the outer rows intact (the framework issues
//    `ROLLBACK TO SAVEPOINT sp_<depth>` + `RELEASE SAVEPOINT`).
// 4. `on_commit` callbacks fire in FIFO order after the outermost
//    commit, never on rollback.
// 5. Callbacks registered inside a nested `atomic()` that rolled back
//    are discarded — only the outer-scope callbacks fire.
// 6. Prefetch stitching works inside `atomic()` — proves the generalised
//    `PrefetchLoaderFn` threads `&mut ContextInner` correctly through
//    transaction-backed contexts.
//
// # Closure shape — `Box::pin(async move { ... })`
//
// `atomic()` takes a `for<'a> FnOnce(&'a mut DjogiContext) ->
// AtomicFuture<'a, R>` closure where `AtomicFuture<'a, R>` is a
// `Pin<Box<dyn Future<...> + Send + 'a>>`. This is the same pattern
// framework transaction scopes use — it avoids the "async closure
// implementation not general enough" HRTB inference limitation today's
// compiler hits on bare `AsyncFnOnce` closures whose bodies reborrow
// from the closure argument.
//
// # Fixture strategy
//
// Each test provisions the Phase 4 tables through `#[djogi_test(sync_models = [...])]`.

use djogi::auth::AuthContext;
use djogi::prelude::*;
use djogi::transaction::{
    AtomicFuture, TransactionRetryBackoff, atomic, retry_on_conflict,
    retry_on_conflict_with_backoff,
};
use futures::FutureExt;
use std::future::{Future, pending};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// Test models
// ---------------------------------------------------------------------------

// Phase 7-Zero-2 T2 default flip — pin ascending HeerId across these
// models so HeerId-typed construction and cross-model FK relations
// (`ledger_id: ForeignKey<Ledger>` etc.) stay homogeneous.
#[model(table = "accounts", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Account {
    pub balance: i64,
    /// Per-account overdraft cap. Task 3a's `field_vs_field_filter`
    /// test compares `balance` against this column as an
    /// `Expr<bool>` predicate; other Phase 4 tests leave it at the
    /// `Default::default()` value (0) and do not touch it.
    pub overdraft_limit: i64,
    /// Human-readable status label — the Task 5 CASE-backed UPDATE
    /// test populates this from a `Case::when(...).otherwise(...)`
    /// expression ("overdrawn" vs "ok"). Defaults to the empty
    /// string so every pre-Task-5 test can keep using
    /// `Account { balance: X, ..Default::default() }` without
    /// spelling the new field.
    pub status: String,
}

// Parent / child pair for the prefetch-inside-atomic test. The `_p4`
// suffix keeps these isolated from the Phase 3 prefetch fixtures.
#[model(table = "ledgers_p4", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Ledger {
    pub name: String,
}

#[model(table = "entries_p4", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Entry {
    pub ledger_id: ForeignKey<Ledger>,
    pub memo: String,
}

// Events-enabled model for Phase 4 Task 6. `kind` is the payload-
// visible column; `internal_notes` is excluded from the outbox payload
// via `#[field(outbox = "ignore")]`. Kept separate from `Account` so
// Tasks 1-5 assertions that count rows in `accounts_outbox` stay
// unaffected (non-events models write nothing there).
#[model(table = "notifications", pk = HeerId, events)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct Notification {
    pub kind: String,
    #[field(
        outbox = "ignore",
        rationale = "internal operator commentary — should never leak to downstream consumers"
    )]
    pub internal_notes: Option<String>,
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

const NOTIFICATIONS_OUTBOX: &str = "notifications_outbox";

fn entry_for_insert(memo: &str, ledger: &Ledger) -> Entry {
    Entry {
        id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
        created_at: ::djogi::types::DateTime::UNIX_EPOCH,
        updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
        ledger_id: ForeignKey::new(ledger.id),
        memo: memo.into(),
    }
}

async fn clear_notification_outbox(ctx: &mut djogi::DjogiContext) {
    djogi::testing::clear_outbox_for_test(ctx, NOTIFICATIONS_OUTBOX)
        .await
        .expect("clear notifications_outbox rows");
}

async fn notification_outbox_rows(
    ctx: &mut djogi::DjogiContext,
) -> Vec<djogi::testing::OutboxRowForTest> {
    djogi::testing::outbox_rows_for_test(ctx, NOTIFICATIONS_OUTBOX)
        .await
        .expect("load notifications_outbox rows")
}

async fn run_atomic<F, R>(ctx: &mut djogi::DjogiContext, closure: F) -> Result<R, DjogiError>
where
    R: Send + 'static,
    F: for<'a> FnOnce(&'a mut djogi::DjogiContext) -> AtomicFuture<'a, R> + Send,
{
    let mut tx = ctx.begin().await?;
    let result = atomic(&mut tx, closure).await;
    match result {
        Ok(value) => {
            tx.commit().await?;
            Ok(value)
        }
        Err(err) => {
            let _ = tx.rollback().await;
            Err(err)
        }
    }
}

async fn run_single_connection_phase4_fixture<F, Fut>(test: F)
where
    F: FnOnce(djogi::pg::pool::DjogiPool, djogi::DjogiContext) -> Fut,
    Fut: Future<Output = ()>,
{
    let (cleanup, mut setup_ctx) = djogi::testing::setup_test_db()
        .await
        .expect("setup_test_db must provision a phase4 cancellation fixture");
    djogi::testing::sync_models(
        &mut setup_ctx,
        &[
            <Account as djogi::model::Model>::descriptor(),
            <Ledger as djogi::model::Model>::descriptor(),
            <Entry as djogi::model::Model>::descriptor(),
            <Notification as djogi::model::Model>::descriptor(),
        ],
    )
    .await
    .expect("sync phase4 models");
    let test_url = cleanup
        .test_url()
        .expect("cleanup token must produce a per-test database URL");
    drop(setup_ctx);

    let pool = djogi::pg::pool::DjogiPool::builder(&test_url)
        .max_size(1)
        .build()
        .await
        .expect("single-connection phase4 pool must build");
    let ctx = djogi::DjogiContext::from_pool(pool.clone());

    let outcome = AssertUnwindSafe(test(pool, ctx)).catch_unwind().await;
    djogi::testing::teardown_test_db(cleanup).await;
    if let Err(panic_payload) = outcome {
        std::panic::resume_unwind(panic_payload);
    }
}

async fn run_single_connection_phase4_fixture_with_timeout<F, Fut>(timeout: Duration, test: F)
where
    F: FnOnce(djogi::pg::pool::DjogiPool, djogi::DjogiContext) -> Fut,
    Fut: Future<Output = ()>,
{
    let (cleanup, mut setup_ctx) = djogi::testing::setup_test_db()
        .await
        .expect("setup_test_db must provision a phase4 cancellation fixture");
    djogi::testing::sync_models(
        &mut setup_ctx,
        &[
            <Account as djogi::model::Model>::descriptor(),
            <Ledger as djogi::model::Model>::descriptor(),
            <Entry as djogi::model::Model>::descriptor(),
            <Notification as djogi::model::Model>::descriptor(),
        ],
    )
    .await
    .expect("sync phase4 models");
    let test_url = cleanup
        .test_url()
        .expect("cleanup token must produce a per-test database URL");
    drop(setup_ctx);

    let pool = djogi::pg::pool::DjogiPool::builder(&test_url)
        .max_size(1)
        .timeout(timeout)
        .build()
        .await
        .expect("single-connection phase4 pool with timeout must build");
    let ctx = djogi::DjogiContext::from_pool(pool.clone());

    let outcome = AssertUnwindSafe(test(pool, ctx)).catch_unwind().await;
    djogi::testing::teardown_test_db(cleanup).await;
    if let Err(panic_payload) = outcome {
        std::panic::resume_unwind(panic_payload);
    }
}

// ---------------------------------------------------------------------------
// Task 1 integration tests — atomic() / savepoints / on_commit
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn atomic_commits_on_success(mut ctx: djogi::DjogiContext) {
    run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            Account::create(
                ctx,
                Account {
                    balance: 100,
                    ..Default::default()
                },
            )
            .await?;
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("atomic should commit on Ok");

    let count: i64 = Account::objects().count(&mut ctx).await.unwrap();
    assert_eq!(count, 1, "committed row must be visible after the scope");
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn atomic_rolls_back_on_err(mut ctx: djogi::DjogiContext) {
    let res = run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            Account::create(
                ctx,
                Account {
                    balance: 100,
                    ..Default::default()
                },
            )
            .await?;
            Err::<(), _>(DjogiError::not_found("forced"))
        })
    })
    .await;
    assert!(res.is_err(), "closure returned Err must surface");

    let count: i64 = Account::objects().count(&mut ctx).await.unwrap();
    assert_eq!(count, 0, "rollback must leave no rows");
}

#[tokio::test]
async fn atomic_pool_context_cancellation_detaches_dirty_connection() {
    run_single_connection_phase4_fixture(|pool, mut ctx| async move {
        ctx.set_auth(
            AuthContext::new(djogi::HeerId::from_i64(7).expect("HeerId(7) is valid"))
                .with_tenant("1000"),
        );
        let auth_before = ctx.auth().cloned().expect("parent auth snapshot");
        let tenant_scope_before = ctx.__tenant_scope_suppressed_for_macros();
        ctx.set_tenant("stage1-parent")
            .await
            .expect("prime parent transaction trackers");
        assert!(ctx.tenant_set, "parent tracker should be primed");
        assert_eq!(ctx.applied_tenant_id(), Some("stage1-parent"));

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        {
            let fut = atomic(&mut ctx, |tx| {
                Box::pin(async move {
                    Account::create(
                        tx,
                        Account {
                            balance: 281_001,
                            ..Default::default()
                        },
                    )
                    .await?;
                    tx.set_auth(
                        AuthContext::new(djogi::HeerId::from_i64(8).expect("HeerId(8) is valid"))
                            .with_tenant("2000"),
                    );
                    tx.set_no_tenant_scope();
                    let _ = ready_tx.send(());
                    pending::<()>().await;
                    #[allow(unreachable_code)]
                    Ok::<_, DjogiError>(())
                })
            });
            tokio::pin!(fut);

            tokio::select! {
                result = &mut fut => panic!("atomic future completed before cancellation: {result:?}"),
                ready = ready_rx => ready.expect("dirty transaction should signal readiness"),
            };

            let result = tokio::time::timeout(Duration::from_millis(25), &mut fut).await;
            assert!(
                result.is_err(),
                "timeout must drop the dirty top-level pool-context atomic future"
            );
        }

        let status = pool.status();
        assert_eq!(
            status.size, 0,
            "cancelled dirty atomic(&mut pool_ctx, ...) must detach the \
             physical connection; pool.size should drop to 0, got: {status:?}"
        );
        assert!(
            !ctx.tenant_set,
            "pool-context cancellation must clear parent tenant_set tracker"
        );
        assert_eq!(
            ctx.applied_tenant_id(),
            None,
            "pool-context cancellation must clear parent applied_tenant_id tracker"
        );
        let auth_after = ctx.auth().expect("parent auth must remain attached");
        assert_eq!(
            auth_after.user_id, auth_before.user_id,
            "pool-context cancellation must not mutate parent auth user_id"
        );
        assert_eq!(
            auth_after.tenant_id,
            auth_before.tenant_id,
            "pool-context cancellation must not mutate parent auth tenant_id"
        );
        assert_eq!(
            auth_after.scopes,
            auth_before.scopes,
            "pool-context cancellation must not mutate parent auth scopes"
        );
        assert_eq!(
            auth_after.ext, auth_before.ext,
            "pool-context cancellation must not mutate parent auth ext"
        );
        assert_eq!(
            ctx.__tenant_scope_suppressed_for_macros(),
            tenant_scope_before,
            "pool-context cancellation must not mutate parent tenant-scope suppression"
        );

        let count_after_cancel: i64 = Account::objects()
            .count(&mut ctx)
            .await
            .expect("parent ctx should remain usable after cancellation");
        assert_eq!(
            count_after_cancel, 0,
            "cancelled closure-body write must be rolled back by detach"
        );

        atomic(&mut ctx, |tx| {
            Box::pin(async move {
                Account::create(
                    tx,
                    Account {
                        balance: 281_002,
                        ..Default::default()
                    },
                )
                .await?;
                Ok::<_, DjogiError>(())
            })
        })
        .await
        .expect("normal work should succeed after cancellation");

        let rows = Account::objects()
            .fetch_all(&mut ctx)
            .await
            .expect("fetch rows after recovery");
        let balances: Vec<i64> = rows.into_iter().map(|row| row.balance).collect();
        assert_eq!(
            balances,
            vec![281_002],
            "cancelled row must be absent and recovery row must commit"
        );
    })
    .await;
}

#[tokio::test]
async fn atomic_pool_reference_cancellation_detaches_dirty_connection() {
    run_single_connection_phase4_fixture(|pool, mut ctx| async move {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        {
            let fut = atomic(&pool, |tx| {
                Box::pin(async move {
                    Account::create(
                        tx,
                        Account {
                            balance: 281_101,
                            ..Default::default()
                        },
                    )
                    .await?;
                    let _ = ready_tx.send(());
                    pending::<()>().await;
                    #[allow(unreachable_code)]
                    Ok::<_, DjogiError>(())
                })
            });
            tokio::pin!(fut);

            tokio::select! {
                result = &mut fut => panic!("atomic future completed before cancellation: {result:?}"),
                ready = ready_rx => ready.expect("dirty transaction should signal readiness"),
            };

            let result = tokio::time::timeout(Duration::from_millis(25), &mut fut).await;
            assert!(
                result.is_err(),
                "timeout must drop the dirty top-level pool-reference atomic future"
            );
        }

        let status = pool.status();
        assert_eq!(
            status.size, 0,
            "cancelled dirty atomic(&pool, ...) must detach the physical \
             connection; pool.size should drop to 0, got: {status:?}"
        );

        let count_after_cancel: i64 = Account::objects()
            .count(&mut ctx)
            .await
            .expect("pool should recover after pool-reference cancellation");
        assert_eq!(
            count_after_cancel, 0,
            "cancelled pool-reference write must be absent"
        );

        atomic(&pool, |tx| {
            Box::pin(async move {
                Account::create(
                    tx,
                    Account {
                        balance: 281_102,
                        ..Default::default()
                    },
                )
                .await?;
                Ok::<_, DjogiError>(())
            })
        })
        .await
        .expect("pool-reference atomic should still work after cancellation");

        let rows = Account::objects()
            .fetch_all(&mut ctx)
            .await
            .expect("fetch rows after pool-reference recovery");
        let balances: Vec<i64> = rows.into_iter().map(|row| row.balance).collect();
        assert_eq!(
            balances,
            vec![281_102],
            "cancelled pool-reference row must be absent and later work must commit"
        );
    })
    .await;
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn nested_atomic_cancellation_poisons_outer_transaction(mut ctx: djogi::DjogiContext) {
    let callbacks = Arc::new(AtomicUsize::new(0));

    let outer_result = {
        let callbacks = Arc::clone(&callbacks);
        atomic(&mut ctx, |outer| {
            Box::pin(async move {
                Account::create(
                    outer,
                    Account {
                        balance: 281_201,
                        ..Default::default()
                    },
                )
                .await?;
                outer.set_auth(
                    AuthContext::new(djogi::HeerId::from_i64(281).expect("HeerId(281) is valid"))
                        .with_tenant("outer-tenant"),
                );
                outer.set_tenant("outer-tenant").await?;
                let auth_before = outer.auth().cloned().expect("outer auth snapshot");
                let tenant_scope_before = outer.__tenant_scope_suppressed_for_macros();
                let tenant_set_before = outer.tenant_set;
                let applied_tenant_before = outer.applied_tenant_id().map(str::to_owned);

                {
                    let callbacks = Arc::clone(&callbacks);
                    outer.on_commit(move || async move {
                        callbacks.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    });
                }

                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
                {
                    let callbacks = Arc::clone(&callbacks);
                    let inner = atomic(&mut *outer, |inner| {
                        Box::pin(async move {
                            Account::create(
                                inner,
                                Account {
                                    balance: 281_202,
                                    ..Default::default()
                                },
                            )
                            .await?;
                            inner.set_auth(
                                AuthContext::new(
                                    djogi::HeerId::from_i64(282).expect("HeerId(282) is valid"),
                                )
                                .with_tenant("inner-tenant"),
                            );
                            inner.set_tenant("inner-tenant").await?;
                            inner.set_no_tenant_scope();
                            inner.on_commit(move || async move {
                                callbacks.fetch_add(1, Ordering::SeqCst);
                                Ok(())
                            });
                            let _ = ready_tx.send(());
                            pending::<()>().await;
                            #[allow(unreachable_code)]
                            Ok::<_, DjogiError>(())
                        })
                    });
                    tokio::pin!(inner);

                    tokio::select! {
                        result = &mut inner => {
                            panic!("nested atomic future completed before cancellation: {result:?}")
                        }
                        ready = ready_rx => ready.expect("inner transaction should signal readiness"),
                    }

                    let timeout = tokio::time::timeout(Duration::from_millis(25), &mut inner).await;
                    assert!(
                        timeout.is_err(),
                        "timeout must leave the nested atomic future to be dropped"
                    );
                }

                let auth_after = outer.auth().expect("outer auth must be restored");
                assert_eq!(auth_after.user_id, auth_before.user_id);
                assert_eq!(auth_after.tenant_id, auth_before.tenant_id);
                assert_eq!(auth_after.scopes, auth_before.scopes);
                assert_eq!(auth_after.ext, auth_before.ext);
                assert_eq!(
                    outer.__tenant_scope_suppressed_for_macros(),
                    tenant_scope_before
                );
                assert_eq!(outer.tenant_set, tenant_set_before);
                assert_eq!(
                    outer.applied_tenant_id(),
                    applied_tenant_before.as_deref()
                );

                let later_query = Account::objects().count(outer).await;
                assert!(
                    matches!(
                        later_query,
                        Err(DjogiError::TransactionPoisoned {
                            reason: "nested atomic future dropped before savepoint cleanup",
                            ..
                        })
                    ),
                    "framework-owned work after nested cancellation must return TransactionPoisoned, got: {later_query:?}"
                );

                let joined_err = Entry::objects()
                    .select_related(EntryRelated::ledger())
                    .fetch_all_joined(outer)
                    .await;
                assert!(
                    matches!(
                        joined_err,
                        Err(DjogiError::TransactionPoisoned {
                            reason: "nested atomic future dropped before savepoint cleanup",
                            ..
                        })
                    ),
                    "joined fetch after nested cancellation must return TransactionPoisoned, got: {joined_err:?}"
                );

                let prefetch_err = Entry::objects()
                    .prefetch(EntryRelated::ledger())
                    .fetch_all_prefetched(outer)
                    .await;
                assert!(
                    matches!(
                        prefetch_err,
                        Err(DjogiError::TransactionPoisoned {
                            reason: "nested atomic future dropped before savepoint cleanup",
                            ..
                        })
                    ),
                    "prefetch fetch after nested cancellation must return TransactionPoisoned, got: {prefetch_err:?}"
                );

                Ok::<_, DjogiError>(())
            })
        })
        .await
    };

    assert!(
        matches!(
            outer_result,
            Err(DjogiError::TransactionPoisoned {
                reason: "nested atomic future dropped before savepoint cleanup",
                ..
            })
        ),
        "outer atomic commit must fail closed with TransactionPoisoned, got: {outer_result:?}"
    );

    let rows = Account::objects()
        .fetch_all(&mut ctx)
        .await
        .expect("pool context should be usable after poisoned outer rollback");
    let balances: Vec<i64> = rows.into_iter().map(|row| row.balance).collect();
    assert!(
        !balances.contains(&281_201) && !balances.contains(&281_202),
        "poisoned outer transaction must roll back both outer and inner rows, got: {balances:?}"
    );
    assert_eq!(
        callbacks.load(Ordering::SeqCst),
        0,
        "callbacks from poisoned transaction must not fire"
    );
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn nested_atomic_uses_savepoints(mut ctx: djogi::DjogiContext) {
    run_atomic(&mut ctx, |outer| {
        Box::pin(async move {
            Account::create(
                outer,
                Account {
                    balance: 10,
                    ..Default::default()
                },
            )
            .await?;

            let inner_res = atomic(&mut *outer, |inner| {
                Box::pin(async move {
                    Account::create(
                        inner,
                        Account {
                            balance: 20,
                            ..Default::default()
                        },
                    )
                    .await?;
                    Err::<(), _>(DjogiError::not_found("inner fail"))
                })
            })
            .await;
            assert!(inner_res.is_err());

            // Outer row still there; inner row rolled back via SAVEPOINT
            // ROLLBACK TO / RELEASE pair.
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("outer atomic must still commit after nested rollback");

    let count: i64 = Account::objects().count(&mut ctx).await.unwrap();
    assert_eq!(count, 1, "only the outer row survives the nested rollback");
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn nested_atomic_cancellation_poisoned_outer_transaction_rolls_back_all_work(
    mut ctx: djogi::DjogiContext,
) {
    ctx.set_auth(
        AuthContext::new(djogi::HeerId::from_i64(7).expect("HeerId(7) is valid"))
            .with_tenant("1000"),
    );
    ctx.set_tenant("stage1-parent")
        .await
        .expect("parent tracker priming must succeed");
    assert!(ctx.tenant_set, "parent tracker should be primed");
    assert_eq!(ctx.applied_tenant_id(), Some("stage1-parent"));

    let callback_count = Arc::new(AtomicUsize::new(0));

    let outer_result = {
        let callback_count = callback_count.clone();
        atomic(&mut ctx, |outer| {
            Box::pin(async move {
                outer.set_auth(
                    AuthContext::new(djogi::HeerId::from_i64(7).expect("HeerId(7) is valid"))
                        .with_tenant("1000"),
                );
                let auth_before = outer.auth().cloned().expect("outer auth snapshot");
                let tenant_scope_before = outer.__tenant_scope_suppressed_for_macros();

                outer
                    .set_tenant("1000")
                    .await
                    .expect("outer tenant priming must succeed");
                let tenant_set_before = outer.tenant_set;
                let applied_tenant_before = outer.applied_tenant_id().map(str::to_owned);

                Account::create(
                    outer,
                    Account {
                        balance: 281_201,
                        ..Default::default()
                    },
                )
                .await?;

                {
                    let callback_count = callback_count.clone();
                    outer.on_commit(move || async move {
                        callback_count.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    });
                }

                let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
                let inner_result = {
                    let fut = atomic(&mut *outer, |inner| {
                        Box::pin(async move {
                            Account::create(
                                inner,
                                Account {
                                    balance: 281_202,
                                    ..Default::default()
                                },
                            )
                            .await?;
                            inner.set_auth(
                                AuthContext::new(
                                    djogi::HeerId::from_i64(8).expect("HeerId(8) is valid"),
                                )
                                .with_tenant("2000"),
                            );
                            inner.set_no_tenant_scope();
                            inner
                                .set_tenant("2000")
                                .await
                                .expect("inner tenant mutation must succeed");
                            {
                                let callback_count = callback_count.clone();
                                inner.on_commit(move || async move {
                                    callback_count.fetch_add(10, Ordering::SeqCst);
                                    Ok(())
                                });
                            }
                            let _ = ready_tx.send(());
                            pending::<()>().await;
                            #[allow(unreachable_code)]
                            Ok::<_, DjogiError>(())
                        })
                    });
                    tokio::pin!(fut);

                    tokio::select! {
                        result = &mut fut => panic!("inner atomic completed before cancellation: {result:?}"),
                        ready = ready_rx => ready.expect("inner atomic should signal dirty readiness"),
                    };

                    tokio::time::timeout(Duration::from_millis(25), &mut fut).await
                };
                assert!(
                    inner_result.is_err(),
                    "timeout must drop the nested atomic future before it resolves"
                );

                assert_eq!(
                    outer.tenant_set,
                    tenant_set_before,
                    "nested cancellation must restore outer tenant_set tracker"
                );
                assert_eq!(
                    outer.applied_tenant_id().map(str::to_owned),
                    applied_tenant_before,
                    "nested cancellation must restore outer applied_tenant_id"
                );
                let auth_after = outer.auth().expect("outer auth must remain attached");
                assert_eq!(
                    auth_after.user_id, auth_before.user_id,
                    "nested cancellation must restore outer auth user_id"
                );
                assert_eq!(
                    auth_after.tenant_id,
                    auth_before.tenant_id,
                    "nested cancellation must restore outer auth tenant_id"
                );
                assert_eq!(
                    auth_after.scopes,
                    auth_before.scopes,
                    "nested cancellation must restore outer auth scopes"
                );
                assert_eq!(
                    auth_after.ext, auth_before.ext,
                    "nested cancellation must restore outer auth ext"
                );
                assert_eq!(
                    outer.__tenant_scope_suppressed_for_macros(),
                    tenant_scope_before,
                    "nested cancellation must restore outer tenant-scope suppression"
                );

                let poison_err = Account::objects()
                    .count(outer)
                    .await
                    .expect_err("poisoned outer context must reject further helper-path work");
                assert!(
                    matches!(poison_err, DjogiError::TransactionPoisoned { .. }),
                    "expected TransactionPoisoned after nested cancellation, got: {poison_err:?}"
                );

                Ok::<_, DjogiError>(())
            })
        })
        .await
    };
    let outer_err = outer_result.expect_err("outer transaction must refuse commit after poison");
    assert!(
        matches!(outer_err, DjogiError::TransactionPoisoned { .. }),
        "expected TransactionPoisoned on outer commit, got: {outer_err:?}"
    );
    assert!(
        !ctx.tenant_set,
        "poisoned outer commit must clear parent tenant_set tracker"
    );
    assert_eq!(
        ctx.applied_tenant_id(),
        None,
        "poisoned outer commit must clear parent applied_tenant_id tracker"
    );

    let count: i64 = Account::objects()
        .count(&mut ctx)
        .await
        .expect("rolled-back outer transaction should leave parent ctx usable");
    assert_eq!(
        count, 0,
        "poisoned outer transaction must roll back both outer and inner writes"
    );
    assert_eq!(
        callback_count.load(Ordering::SeqCst),
        0,
        "neither outer nor inner on_commit callbacks may fire after nested cancellation"
    );
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn on_commit_fires_after_outer_commit(mut ctx: djogi::DjogiContext) {
    let flag = Arc::new(AtomicBool::new(false));

    {
        let flag = flag.clone();
        run_atomic(&mut ctx, |ctx| {
            Box::pin(async move {
                Account::create(
                    ctx,
                    Account {
                        balance: 1,
                        ..Default::default()
                    },
                )
                .await?;
                ctx.on_commit(move || async move {
                    flag.store(true, Ordering::SeqCst);
                    Ok(())
                });
                Ok::<_, DjogiError>(())
            })
        })
        .await
        .unwrap();
    }

    assert!(
        flag.load(Ordering::SeqCst),
        "on_commit must fire after outer commit"
    );
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn on_commit_does_not_fire_on_rollback(mut ctx: djogi::DjogiContext) {
    let flag = Arc::new(AtomicBool::new(false));

    let _res = {
        let flag = flag.clone();
        run_atomic(&mut ctx, |ctx| {
            Box::pin(async move {
                ctx.on_commit(move || async move {
                    flag.store(true, Ordering::SeqCst);
                    Ok(())
                });
                Err::<(), _>(DjogiError::not_found("forced"))
            })
        })
        .await
    };

    assert!(
        !flag.load(Ordering::SeqCst),
        "on_commit must NOT fire when outer rolls back"
    );
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn savepoint_rollback_discards_inner_on_commit(mut ctx: djogi::DjogiContext) {
    let count = Arc::new(AtomicUsize::new(0));

    {
        let count = count.clone();
        run_atomic(&mut ctx, |outer| {
            Box::pin(async move {
                {
                    let c = count.clone();
                    outer.on_commit(move || async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    });
                }

                let _ = atomic(&mut *outer, |inner| {
                    Box::pin(async move {
                        {
                            let c = count.clone();
                            inner.on_commit(move || async move {
                                c.fetch_add(10, Ordering::SeqCst);
                                Ok(())
                            });
                        }
                        Err::<(), _>(DjogiError::not_found("inner fail"))
                    })
                })
                .await;
                Ok::<_, DjogiError>(())
            })
        })
        .await
        .unwrap();
    }

    // Outer commit fires +1; inner rollback discards the +10.
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn nested_atomic_on_commit_promotes_to_outer(mut ctx: djogi::DjogiContext) {
    let count = Arc::new(AtomicUsize::new(0));

    {
        let count = count.clone();
        run_atomic(&mut ctx, |outer| {
            Box::pin(async move {
                atomic(&mut *outer, |inner| {
                    Box::pin(async move {
                        let c = count.clone();
                        inner.on_commit(move || async move {
                            c.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        });
                        Ok::<_, DjogiError>(())
                    })
                })
                .await?;
                Ok::<_, DjogiError>(())
            })
        })
        .await
        .unwrap();
    }

    // Inner-registered callback was promoted to the outer queue on
    // nested Ok, then drained after the outer commit.
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "inner on_commit registered during nested Ok must fire after outer commit"
    );
}

// ---------------------------------------------------------------------------
// Retry helpers — immediate-retry and public backoff surface. Actual
// row-lock conflict semantics need a real concurrent scenario and are
// exercised in Task 7 (row locks); the backoff helper's public
// PoolTimeout path is covered here with a saturated single-connection
// fixture.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn retry_on_conflict_does_not_retry_on_success(mut ctx: djogi::DjogiContext) {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let result = retry_on_conflict(&mut ctx, 3, async move |_ctx| {
        c.fetch_add(1, Ordering::SeqCst);
        Ok::<i32, DjogiError>(42)
    })
    .await
    .unwrap();

    assert_eq!(result, 42);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "closure that returns Ok on the first call must run exactly once"
    );
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn retry_on_conflict_short_circuits_on_non_lock_error(mut ctx: djogi::DjogiContext) {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let result = retry_on_conflict(&mut ctx, 5, async move |_ctx| {
        c.fetch_add(1, Ordering::SeqCst);
        Err::<(), _>(DjogiError::not_found("forced"))
    })
    .await;

    assert!(result.is_err(), "non-lock error must surface");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "non-lock errors must not retry regardless of attempts budget"
    );
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn retry_on_conflict_with_backoff_does_not_retry_on_success(mut ctx: djogi::DjogiContext) {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let result = retry_on_conflict_with_backoff(
        &mut ctx,
        3,
        TransactionRetryBackoff::none(),
        async move |_ctx| {
            c.fetch_add(1, Ordering::SeqCst);
            Ok::<i32, DjogiError>(42)
        },
    )
    .await
    .unwrap();

    assert_eq!(result, 42);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "closure that returns Ok on the first call must run exactly once"
    );
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn retry_on_conflict_with_backoff_short_circuits_on_terminal_error(
    mut ctx: djogi::DjogiContext,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let result = retry_on_conflict_with_backoff(
        &mut ctx,
        5,
        TransactionRetryBackoff::none(),
        async move |_ctx| {
            c.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(DjogiError::not_found("forced"))
        },
    )
    .await;

    assert!(result.is_err(), "terminal error must surface");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "terminal errors must not retry regardless of attempts budget"
    );
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn retry_on_conflict_with_backoff_retries_pool_timeout_and_recovers(
    mut ctx: djogi::DjogiContext,
) {
    let _ = &mut ctx;
    run_single_connection_phase4_fixture_with_timeout(
        Duration::from_millis(50),
        |pool, mut retry_ctx| async move {
            let (ready_tx, ready_rx) = oneshot::channel::<()>();
            let (release_tx, release_rx) = oneshot::channel::<()>();

            let mut holder_ctx = djogi::DjogiContext::from_pool(pool);
            let hold_join = tokio::spawn(async move {
                atomic(&mut holder_ctx, |tx| {
                    Box::pin(async move {
                        let _ = ready_tx.send(());
                        let _ = release_rx.await;
                        let _ = tx;
                        Ok::<_, DjogiError>(())
                    })
                })
                .await
            });

            ready_rx
                .await
                .expect("holder transaction must confirm the only connection is checked out");

            let calls = Arc::new(AtomicUsize::new(0));
            let c = calls.clone();
            let mut release_tx = Some(release_tx);
            let mut hold_join = Some(hold_join);
            let result = retry_on_conflict_with_backoff(
                &mut retry_ctx,
                2,
                TransactionRetryBackoff::none(),
                async move |ctx| {
                    c.fetch_add(1, Ordering::SeqCst);
                    match Account::objects().count(ctx).await {
                        Err(err @ DjogiError::PoolTimeout { .. }) => {
                            if let Some(tx) = release_tx.take() {
                                let _ = tx.send(());
                            }
                            if let Some(handle) = hold_join.take() {
                                handle
                                    .await
                                    .expect("holder task joins cleanly")
                                    .expect("holder transaction exits cleanly");
                            }
                            Err(err)
                        }
                        other => other,
                    }
                },
            )
            .await
            .expect("retry helper should recover after the held connection is released");

            assert_eq!(result, 0, "no seeded accounts should exist in the fixture");
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "PoolTimeout must be retried once and then recover on the second attempt"
            );
        },
    )
    .await;
}

// ---------------------------------------------------------------------------
// save() rehydration — `UPDATE ... RETURNING *` mutates `self` with
// DB truth so triggers, server-side defaults, and the advanced
// `updated_at` all surface on the receiver. Task 2 scope.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn save_rehydrates_updated_at(mut ctx: djogi::DjogiContext) {
    let mut account = Account::create(
        &mut ctx,
        Account {
            balance: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let first_ts = account.updated_at;

    // The `now()` granularity inside a single Postgres statement is
    // usually finer than the test can observe without a pause. 10ms is
    // overkill for `clock_timestamp()` but stable against the
    // statement-time `now()` the SET clause uses.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    account.balance = 999;
    account.save(&mut ctx).await.unwrap();

    assert!(
        account.updated_at > first_ts,
        "updated_at must advance after save (RETURNING * rehydration)"
    );
    assert_eq!(account.balance, 999);
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn save_reflects_trigger_modified_fields(mut ctx: djogi::DjogiContext) {
    ::djogi::testing::install_accounts_balance_increment_trigger_for_test(&mut ctx)
        .await
        .unwrap();

    let mut account = Account::create(
        &mut ctx,
        Account {
            balance: 100,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    account.balance = 200;
    account.save(&mut ctx).await.unwrap();

    // Trigger incremented by 1 during the UPDATE; RETURNING * rehydrates
    // the receiver with the trigger-adjusted value.
    assert_eq!(account.balance, 201);
}

// ---------------------------------------------------------------------------
// Task 3b: expression-backed UPDATE assignments — `col = col + N`
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn bulk_update_arithmetic_expression(mut ctx: djogi::DjogiContext) {
    // Seed two accounts; wrapping in `atomic()` keeps the transaction scope explicit during the multi-step seed.
    run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            Account::create(
                ctx,
                Account {
                    balance: 10,
                    ..Default::default()
                },
            )
            .await?;
            Account::create(
                ctx,
                Account {
                    balance: 20,
                    ..Default::default()
                },
            )
            .await?;

            // balance = balance + 5 — expression-backed UPDATE.
            let n = Account::objects()
                .update(|f| {
                    f.balance()
                        .set_expr(f.balance().as_expr() + Expr::literal(5i64))
                })
                .execute(ctx)
                .await?;
            assert_eq!(n, 2, "both rows must be updated");

            let balances: Vec<i64> = Account::objects()
                .order_by(|f| f.balance().asc())
                .fetch_all(ctx)
                .await?
                .into_iter()
                .map(|a| a.balance)
                .collect();
            assert_eq!(balances, vec![15, 25]);
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .unwrap();
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn bulk_update_field_to_field_copy(mut ctx: djogi::DjogiContext) {
    run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            Account::create(
                ctx,
                Account {
                    balance: 100,
                    overdraft_limit: 200,
                    ..Default::default()
                },
            )
            .await?;

            // balance = overdraft_limit — field-vs-field assignment.
            let n = Account::objects()
                .update(|f| f.balance().set_expr(f.overdraft_limit().as_expr()))
                .execute(ctx)
                .await?;
            assert_eq!(n, 1);

            let rows = Account::objects().fetch_all(ctx).await?;
            assert_eq!(rows[0].balance, 200);
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .unwrap();
}

// ---------------------------------------------------------------------------
// Prefetch-in-atomic — proves `PrefetchLoaderFn` works over a
// transaction-backed context.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn prefetch_works_inside_atomic(mut ctx: djogi::DjogiContext) {
    // All writes + reads happen inside a single atomic scope. If the
    // prefetch loader still bailed on `ContextInner::Transaction`, this
    // test would fail at the `.fetch_all_prefetched(ctx)` call with a
    // configuration error. The assertion on the resolved relation proves
    // the loader ran over the transaction-backed context and stitched
    // the parent row correctly.
    run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            let ledger = Ledger::create(
                ctx,
                Ledger {
                    name: "main".into(),
                    ..Default::default()
                },
            )
            .await?;

            let _ = Entry::create(ctx, entry_for_insert("opening", &ledger)).await?;

            let rows: Vec<PrefetchedRow<Entry>> = Entry::objects()
                .prefetch(EntryRelated::ledger())
                .fetch_all_prefetched(ctx)
                .await?;

            assert_eq!(rows.len(), 1);
            let loaded_ledger = rows[0]
                .get(EntryRelated::ledger())
                .expect("prefetched ledger should be present for non-null FK");
            assert_eq!(loaded_ledger.name, "main");
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("prefetch inside atomic must succeed over tx-backed context");
}

// ---------------------------------------------------------------------------
// Task 3a — Expression IR core
//
// Pins the field-vs-field comparison path: `filter_expr` accepts a
// closure that returns `Expr<bool>`, and `Account::objects()
//   .filter_expr(|f| f.balance.as_expr().lt(f.overdraft_limit.as_expr()))`
// round-trips through the SQL emitter to a live Postgres query. The
// unit tests in `djogi/src/expr/sql.rs` cover token-level SQL shape
// assertions; this integration test proves the whole pipeline —
// AST construction, `Condition::Expr` wrapping, `emit_expr`, and the
// terminal `fetch_all` — works against a real database.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Task 4 — Aggregate terminal (`SELECT <agg> FROM table [WHERE ...]`).
//
// Three tests pin the observable behaviour of the Task 4 surface:
//
//   * `aggregate_sum` — seeds three balances and asserts
//     `.aggregate(|f| f.balance().sum()).fetch_one(ctx)` returns the
//     expected scalar total. Proves the scalar path end-to-end: closure
//     -> `AggregateExpr<i64>` -> `SELECT SUM(balance) FROM accounts`
//     -> `query_scalar<i64>`.
//
//   * `aggregate_count_with_filter` — seeds four rows with mixed-sign
//     balances and asserts `.count().filter(balance < 0)` returns the
//     count of negative balances. Proves the `FILTER (WHERE ...)`
//     clause threads through the emitter to Postgres and is honoured
//     at scan time.
//
//   * `annotate_single_aggregate` — seeds two rows and asserts the
//     annotation terminal returns `Vec<(Account, i64)>` with each
//     aggregate aligned to its row. Uses a self-column aggregate
//     (`f.balance().sum()`) rather than a reverse-relation aggregate;
//     `f.orders.count()` is deferred to Task 5 along with the
//     reverse-relation aggregate primitive.
//
// All three tests seed + query inside a single `atomic()` scope. Same
// rationale as `bulk_update_arithmetic_expression` / `field_vs_field_filter`:
// `heer.node_id` is pinned at the database level via
// sync-model provisioning, but a context opened before the
// ALTER DATABASE took effect can still be missing the GUC. Opening a
// fresh transaction inside the test grants all seeds + reads the same
// transactional session — predictable and race-free.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn aggregate_sum(mut ctx: djogi::DjogiContext) {
    run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            for b in [10i64, 20, 30] {
                Account::create(
                    ctx,
                    Account {
                        balance: b,
                        ..Default::default()
                    },
                )
                .await?;
            }

            let total: i64 = Account::objects()
                .aggregate(|f| f.balance().sum())
                .fetch_one(ctx)
                .await?;
            assert_eq!(total, 60, "SUM(balance) over three rows must sum literals");
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("aggregate sum scope must succeed");
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn aggregate_count_with_filter(mut ctx: djogi::DjogiContext) {
    run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            // Two negative, two positive — the filtered count should
            // return exactly the negative rows. Using `balance < 0`
            // is the typed expression-IR equivalent of the plan's
            // pseudocode `filter(balance < 0)`.
            for b in [-5i64, 10, 20, -1] {
                Account::create(
                    ctx,
                    Account {
                        balance: b,
                        ..Default::default()
                    },
                )
                .await?;
            }

            let n: i64 = Account::objects()
                .aggregate(|f| {
                    f.balance()
                        .count()
                        .filter(f.balance().as_expr().lt(Expr::literal(0i64)))
                })
                .fetch_one(ctx)
                .await?;
            assert_eq!(
                n, 2,
                "FILTER (WHERE balance < 0) must restrict COUNT to negative balances"
            );
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("aggregate count+filter scope must succeed");
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn annotate_single_aggregate(mut ctx: djogi::DjogiContext) {
    run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            // Two rows — aggregates are per-row when un-grouped, so
            // `SUM(balance)` here rolls up to the full table per row
            // in the absence of a GROUP BY. Task 4 pins the SQL
            // shape; Task 5+ adds grouping. The important thing at
            // this layer is that `Vec<(Account, i64)>` decodes
            // end-to-end — every row carries its own agg slot.
            Account::create(
                ctx,
                Account {
                    balance: 10,
                    ..Default::default()
                },
            )
            .await?;
            Account::create(
                ctx,
                Account {
                    balance: 20,
                    ..Default::default()
                },
            )
            .await?;

            // `f.balance().sum()` — self-column aggregate as a single
            // annotation. The plan's original pseudocode used a
            // reverse-relation aggregate (`f.orders.count()`), but
            // the reverse-relation aggregate primitive is not wired
            // in Phase 3 and is deferred to Task 5. The self-column
            // form still exercises every layer of the Task 4 surface
            // (SELECT-list builder, name-based FromRow decode, typed
            // tuple decode) without pulling Task 5 dependencies into
            // this test.
            let rows: Vec<(Account, i64)> = Account::objects()
                .order_by(|f| f.balance().asc())
                .annotate(|f| f.balance().sum())
                .fetch_all(ctx)
                .await?;

            assert_eq!(rows.len(), 2, "both rows must come back");
            // Because there is no GROUP BY, SUM rolls up to the full
            // table — both rows carry the same scalar.
            assert_eq!(
                rows[0].1, 30,
                "first row's aggregate slot must be total sum"
            );
            assert_eq!(
                rows[1].1, 30,
                "second row's aggregate slot must be total sum"
            );
            // And the model side still decodes normally.
            assert_eq!(rows[0].0.balance, 10);
            assert_eq!(rows[1].0.balance, 20);
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("annotate single aggregate scope must succeed");
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn field_vs_field_filter(mut ctx: djogi::DjogiContext) {
    // Seed + query inside a single `atomic()` scope. The Phase 2
    // historical fixture used raw transaction setup
    // that were open before `ALTER DATABASE ... SET heer.node_id = '1'`
    // took effect. `atomic()` threads the same kind of transactional
    // session through `DjogiContext::Transaction` so the expression-IR
    // entry point exercises the same tx-backed code path Phase 2 uses.
    //
    // Seed three rows spanning the three comparison outcomes relative
    // to `balance < overdraft_limit`:
    //   row A: balance 50, overdraft 100 -> matches (overdrawn).
    //   row B: balance 100, overdraft 100 -> does not match (equal).
    //   row C: balance 200, overdraft 100 -> does not match (surplus).
    run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            for (balance, overdraft_limit) in [(50i64, 100i64), (100, 100), (200, 100)] {
                Account::create(
                    ctx,
                    Account {
                        balance,
                        overdraft_limit,
                        ..Default::default()
                    },
                )
                .await?;
            }

            let overdrawn = Account::objects()
                .filter_expr(|f| f.balance().as_expr().lt(f.overdraft_limit().as_expr()))
                .fetch_all(ctx)
                .await?;

            assert_eq!(
                overdrawn.len(),
                1,
                "only the row with balance < overdraft_limit should match"
            );
            assert!(
                overdrawn.iter().all(|a| a.balance < a.overdraft_limit),
                "every returned row must satisfy the predicate post-filter"
            );
            assert_eq!(overdrawn[0].balance, 50);
            assert_eq!(overdrawn[0].overdraft_limit, 100);
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("atomic scope around the Task 3a fixture must succeed");
}

// ---------------------------------------------------------------------------
// Task 5 — Subqueries + EXISTS + typed OuterRef + CASE/WHEN
//
// Three integration tests pin the Task 5 surface against live Postgres:
//
//   * `exists_correlated_subquery` — seeds two ledgers with differing
//     entry counts and asserts that an `Exists::new(Entry::objects()
//     .filter_expr(|e| e.ledger_id().as_pk_expr().eq(LedgerOuterRef::id().as_expr())))`
//     predicate returns only the ledger with at least one entry.
//     Exercises the full pipeline: `OuterRef` construction via the
//     macro-emitted `{Model}OuterRef` helper, typed `Expr<HeerId>`
//     correlation through `as_pk_expr` + `.as_expr()`, and
//     `EXISTS (SELECT 1 FROM ... WHERE ...)` emission.
//
//   * `case_when_update` — seeds three rows with distinct balance/
//     overdraft combinations and UPDATEs the `status` column via a
//     `Case::when(balance < 0, "overdrawn").otherwise("ok")` expression.
//     Asserts each row's status matches the correct arm — proves the
//     CASE builder + the required-`otherwise` type-state transition
//     compose end-to-end with the Task 3b expression-backed UPDATE
//     path.
//
//   * `scalar_subquery_in_filter` — seeds a parent row plus a
//     reference-column row and asserts that filtering the parent
//     table against a scalar subquery (`WHERE id = (SELECT id FROM
//     entries WHERE memo = 'opening' LIMIT <implicit>)`) returns the
//     matching parent. Pins the scalar-subquery path separately from
//     EXISTS so a regression in one does not mask the other.
//
// All three tests seed + query inside a single `atomic()` scope for
// the same rationale as the Task 3a / 3b tests above: `heer.node_id`
// is pinned at the database level, but a fresh transaction grants all
// seeds + reads the same transactional session with no race window.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn exists_correlated_subquery(mut ctx: djogi::DjogiContext) {
    run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            // Seed two ledgers with distinct names, and one entry
            // whose memo matches only the first ledger's name. The
            // correlated EXISTS predicate uses the outer-scope
            // `name` column (only `ledgers_p4` has a `name` column —
            // `entries_p4` does not, so there is no same-named
            // collision for Postgres to resolve inward). That lets
            // the unqualified outer-ref emission work correctly here
            // without the qualified-column-ref extension deferred to
            // a later phase. See the rustdoc on
            // [`OuterRef::as_expr`] for the collision limitation.
            let _ledger_a = Ledger::create(
                ctx,
                Ledger {
                    name: "target".into(),
                    ..Default::default()
                },
            )
            .await?;
            let ledger_b = Ledger::create(
                ctx,
                Ledger {
                    name: "empty".into(),
                    ..Default::default()
                },
            )
            .await?;
            // Entry references ledger_b physically (via ledger_id) but
            // its memo matches ledger_a's name — the EXISTS
            // correlation is on memo <-> outer.name, not on the FK.
            // This deliberately decouples the physical FK graph from
            // the correlation logic so the test pins exactly the
            // EXISTS + OuterRef pipeline.
            Entry::create(ctx, entry_for_insert("target", &ledger_b)).await?;

            // Ledger::objects()
            //     .filter_expr(|_| Exists::new(
            //         Entry::objects().filter_expr(|e|
            //             e.memo().as_expr().eq(LedgerOuterRef::name().as_expr())
            //         )
            //     ).as_expr())
            //
            // Renders approximately:
            //   SELECT * FROM ledgers_p4
            //   WHERE EXISTS (
            //     SELECT 1 FROM entries_p4 WHERE memo = name
            //   )
            //
            // The unqualified `name` resolves against the outer
            // ledger scope because `entries_p4` has no `name` column
            // — Postgres' implicit correlation picks up the outer
            // reference. This matches the rustdoc note on
            // `OuterRef::as_expr` about when unqualified emission is
            // safe.
            let matched =
                Ledger::objects()
                    .filter_expr(|_| {
                        Exists::new(Entry::objects().filter_expr(|e| {
                            e.memo().as_expr().eq(LedgerOuterRef::name().as_expr())
                        }))
                        .as_expr()
                    })
                    .fetch_all(ctx)
                    .await?;

            assert_eq!(
                matched.len(),
                1,
                "only the ledger whose name matches some entry's memo must surface"
            );
            assert_eq!(matched[0].name, "target");
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("EXISTS correlated subquery scope must succeed");
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn case_when_update(mut ctx: djogi::DjogiContext) {
    run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            // Seed three rows that span both CASE arms:
            //   row A: balance -5  -> "overdrawn" (WHEN balance < 0)
            //   row B: balance  0  -> "ok"         (ELSE)
            //   row C: balance 10  -> "ok"         (ELSE)
            for b in [-5i64, 0, 10] {
                Account::create(
                    ctx,
                    Account {
                        balance: b,
                        ..Default::default()
                    },
                )
                .await?;
            }

            // UPDATE accounts SET status = CASE WHEN balance < $1 THEN
            // $2 ELSE $3 END
            //
            // The closure threads `f.status()` as the SET target and
            // a `Case::when(...).otherwise(...)` expression as the
            // right-hand side. The typed builder enforces the
            // `otherwise` arm: `Case::when(...)` alone produces a
            // `CaseBuilder<String>`, and only `.otherwise(..)` lifts it
            // to the `Expr<String>` the `set_expr` slot requires.
            let n = Account::objects()
                .update(|f| {
                    f.status().set_expr(
                        Case::when(
                            f.balance().as_expr().lt(Expr::literal(0i64)),
                            Expr::literal("overdrawn".to_string()),
                        )
                        .otherwise(Expr::literal("ok".to_string())),
                    )
                })
                .execute(ctx)
                .await?;
            assert_eq!(n, 3, "every seeded row must be updated");

            // Verify each row's final status — balance determines the
            // arm that fired.
            let rows = Account::objects()
                .order_by(|f| f.balance().asc())
                .fetch_all(ctx)
                .await?;
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0].balance, -5);
            assert_eq!(rows[0].status, "overdrawn");
            assert_eq!(rows[1].balance, 0);
            assert_eq!(rows[1].status, "ok");
            assert_eq!(rows[2].balance, 10);
            assert_eq!(rows[2].status, "ok");
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("CASE-WHEN UPDATE scope must succeed");
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn scalar_subquery_in_filter(mut ctx: djogi::DjogiContext) {
    run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            // Two ledgers with distinct names. We filter Ledger on
            // its pk against a scalar subquery that projects the
            // target ledger's id from a self-query whose filter
            // uniquely identifies the target by name. The subquery
            // returns one row because `name` is unique across the
            // two seeded rows.
            //
            // Self-query (Ledger::objects inside a Ledger filter) is
            // intentional here — it keeps the types aligned
            // (`Expr<HeerId>` on both sides of the outer `.eq()`)
            // without reaching for `as_pk_expr` on an FK wrapper,
            // which a cross-table scalar subquery would need. The
            // EXISTS test above already covers the FK-correlation
            // surface; this test focuses on the scalar-subquery
            // lowering + the outer `=` composition.
            let _target = Ledger::create(
                ctx,
                Ledger {
                    name: "target".into(),
                    ..Default::default()
                },
            )
            .await?;
            let _other = Ledger::create(
                ctx,
                Ledger {
                    name: "other".into(),
                    ..Default::default()
                },
            )
            .await?;

            // SELECT * FROM ledgers_p4
            //   WHERE id = (SELECT id FROM ledgers_p4 WHERE name = $1)
            //
            // The inner queryset projects `id` via the default
            // `LedgerFields::default().id()` handle; the outer
            // `f.id()` references the same column in the enclosing
            // scope. Postgres distinguishes them by the subquery's
            // parentheses — no correlation is needed here because
            // the subquery's own filter uniquely pins its row.
            let inner_fields = <Ledger as djogi::model::Model>::Fields::default();
            let subq = Subquery::new(
                Ledger::objects().filter(|f| f.name().eq("target".to_string())),
                inner_fields.id(),
            );
            let matched = Ledger::objects()
                .filter_expr(|f| f.id().as_expr().eq(subq.as_expr()))
                .fetch_all(ctx)
                .await?;

            assert_eq!(
                matched.len(),
                1,
                "only the ledger whose id matches the subquery scalar must surface"
            );
            assert_eq!(matched[0].name, "target");
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("scalar subquery filter scope must succeed");
}

// ---------------------------------------------------------------------------
// Task 6 — transactional outbox
// ---------------------------------------------------------------------------
//
// Every test in this block uses `Notification` (the events-enabled
// model) — wiring `#[model(events)]` onto `Account` would affect every
// Tasks 1-5 test that counts rows or inspects tables, so we use a
// dedicated model instead. See the `Notification` definition above for
// the attribute shape.

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn outbox_row_written_on_create_in_atomic(mut ctx: djogi::DjogiContext) {
    // Baseline: one `create` inside `atomic()` produces exactly one
    // outbox row with the expected action and row_id.
    let created = run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            Notification::create(
                ctx,
                Notification {
                    kind: "welcome".to_string(),
                    internal_notes: None,
                    ..Default::default()
                },
            )
            .await
        })
    })
    .await
    .expect("create inside atomic must succeed");

    let rows = notification_outbox_rows(&mut ctx).await;
    assert_eq!(rows.len(), 1, "expected exactly one outbox row");
    let row = &rows[0];
    assert_eq!(
        row.row_id,
        created.id.as_i64().to_string(),
        "outbox row_id must match the primary row's id"
    );
    assert_eq!(
        row.action, "create",
        "outbox action column must record 'create'"
    );
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn outbox_rolled_back_on_err(mut ctx: djogi::DjogiContext) {
    // Returning `Err` from `atomic()` rolls the transaction back,
    // discarding both the primary row AND the outbox companion. This
    // is the core guarantee of the transactional-outbox pattern — the
    // outbox and the primary write share one atomic scope.
    let result = run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            Notification::create(
                ctx,
                Notification {
                    kind: "doomed".to_string(),
                    internal_notes: None,
                    ..Default::default()
                },
            )
            .await?;
            Err::<(), _>(DjogiError::not_found("forced-rollback"))
        })
    })
    .await;
    assert!(result.is_err(), "atomic() must propagate the forced error");

    let primary = Notification::objects().count(&mut ctx).await.unwrap();
    let outbox = notification_outbox_rows(&mut ctx).await.len();
    assert_eq!(primary, 0, "primary row must be rolled back");
    assert_eq!(
        outbox, 0,
        "outbox row must be rolled back with the primary row"
    );
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn outbox_payload_excludes_ignored_fields(mut ctx: djogi::DjogiContext) {
    // `#[field(outbox = "ignore")]` must strip the column from the
    // emitted JSONB payload. Framework-injected columns (id/created_at/
    // updated_at) are expected to remain — they carry no exclusion flag.
    run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            Notification::create(
                ctx,
                Notification {
                    kind: "sensitive".to_string(),
                    internal_notes: Some("do-not-leak".to_string()),
                    ..Default::default()
                },
            )
            .await?;
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("create inside atomic must succeed");

    let rows = notification_outbox_rows(&mut ctx).await;
    assert_eq!(rows.len(), 1, "expected exactly one outbox row");
    let obj = rows[0]
        .payload
        .as_object()
        .expect("payload must serialize as a JSON object");
    assert!(obj.contains_key("kind"), "kind must remain in the payload");
    assert!(
        !obj.contains_key("internal_notes"),
        "outbox = \"ignore\" field must be stripped, got: {obj:?}"
    );
    assert!(
        obj.contains_key("id") && obj.contains_key("created_at"),
        "framework columns must remain visible in the payload"
    );
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn outbox_save_writes_refreshed_payload(mut ctx: djogi::DjogiContext) {
    // After `save()` rehydrates the receiver from `RETURNING *`, the
    // outbox payload must reflect the saved row's current typed state.
    let created = run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            Notification::create(
                ctx,
                Notification {
                    kind: "pending".to_string(),
                    internal_notes: None,
                    ..Default::default()
                },
            )
            .await
        })
    })
    .await
    .expect("create must succeed");

    clear_notification_outbox(&mut ctx).await;

    let mut subject = created.clone();
    let post_save_kind = run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            subject.kind = "acknowledged".to_string();
            subject.save(ctx).await?;
            // `save` rehydrates `subject` from `RETURNING *`. Return the
            // value so the assertion can compare it with the outbox payload.
            Ok::<_, DjogiError>(subject.kind.clone())
        })
    })
    .await
    .expect("save must succeed");

    assert_eq!(
        post_save_kind, "acknowledged",
        "save receiver must reflect the saved value"
    );

    let rows = notification_outbox_rows(&mut ctx).await;
    assert_eq!(rows.len(), 1, "expected exactly one save outbox row");
    let row = &rows[0];
    assert_eq!(
        row.action, "save",
        "save path must record 'save' in the outbox action column"
    );
    let obj = row.payload.as_object().unwrap();
    assert_eq!(
        obj["kind"],
        serde_json::Value::String("acknowledged".to_string()),
        "payload must carry the saved value"
    );
}

#[djogi::djogi_test(sync_models = [Account, Ledger, Entry, Notification])]
async fn outbox_delete_captures_predelete_snapshot(mut ctx: djogi::DjogiContext) {
    // `delete(self, ctx)` consumes `self`, but the outbox row must
    // carry the pre-delete payload — proving the emission happens
    // before `self` is dropped at function scope end.
    let created = run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            Notification::create(
                ctx,
                Notification {
                    kind: "goodbye".to_string(),
                    internal_notes: None,
                    ..Default::default()
                },
            )
            .await
        })
    })
    .await
    .expect("create must succeed");

    clear_notification_outbox(&mut ctx).await;

    run_atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            created.clone().delete(ctx).await?;
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("delete must succeed");

    let rows = notification_outbox_rows(&mut ctx).await;
    assert_eq!(rows.len(), 1, "expected exactly one delete outbox row");
    let row = &rows[0];
    assert_eq!(row.action, "delete");
    let obj = row.payload.as_object().unwrap();
    assert_eq!(
        obj["kind"],
        serde_json::Value::String("goodbye".to_string()),
        "delete payload must be the pre-delete snapshot"
    );

    let remaining = Notification::objects().count(&mut ctx).await.unwrap();
    assert_eq!(
        remaining, 0,
        "primary row must be gone after delete; outbox remains"
    );
}
