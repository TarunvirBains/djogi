//! Phase 4 Task 1 integration tests: `atomic()` + savepoints + on_commit
//! drain + transaction-backed prefetch against live Postgres.
//!
//! What this file pins:
//!
//! 1. `atomic(&pool, |ctx| Box::pin(async move { ... }))` opens a
//!    transaction, runs the closure, and commits on `Ok`. Rows written
//!    inside are visible after the scope returns.
//! 2. Returning `Err` from the closure rolls the transaction back — no
//!    rows survive.
//! 3. Nested `atomic(&mut *ctx, ...)` emits `SAVEPOINT sp_<depth>`. An
//!    inner rollback leaves the outer rows intact (the framework issues
//!    `ROLLBACK TO SAVEPOINT sp_<depth>` + `RELEASE SAVEPOINT`).
//! 4. `on_commit` callbacks fire in FIFO order after the outermost
//!    commit, never on rollback.
//! 5. Callbacks registered inside a nested `atomic()` that rolled back
//!    are discarded — only the outer-scope callbacks fire.
//! 6. Prefetch stitching works inside `atomic()` — proves the generalised
//!    `PrefetchLoaderFn` threads `&mut ContextInner` correctly through
//!    both pool-backed and transaction-backed contexts.
//!
//! # Closure shape — `Box::pin(async move { ... })`
//!
//! `atomic()` takes a `for<'a> FnOnce(&'a mut DjogiContext) ->
//! AtomicFuture<'a, R>` closure where `AtomicFuture<'a, R>` is a
//! `Pin<Box<dyn Future<...> + Send + 'a>>`. This is the same pattern
//! `sqlx::Connection::transaction` uses — it avoids the "async closure
//! implementation not general enough" HRTB inference limitation today's
//! compiler hits on bare `AsyncFnOnce` closures whose bodies reborrow
//! from the closure argument.
//!
//! # Fixture strategy
//!
//! Each test provisions the HeeRanjId schema + the Phase 4 tables via
//! `setup_phase4(&pool)`. `ALTER DATABASE ... SET heer.node_id = '1'`
//! persists the node ID at the database level so every pool connection
//! — including the one `pool.begin()` checks out inside `atomic()` —
//! inherits it. Same rationale as `phase1_model::setup_posts`; see that
//! helper's doc comment for the full explanation.

use djogi::prelude::*;
use djogi::transaction::{atomic, retry_on_conflict};
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Test models
// ---------------------------------------------------------------------------

#[model(table = "accounts")]
#[derive(Debug, Clone)]
pub struct Account {
    pub balance: i64,
    /// Per-account overdraft cap. Task 3a's `field_vs_field_filter`
    /// test compares `balance` against this column as an
    /// `Expr<bool>` predicate; other Phase 4 tests leave it at the
    /// `Default::default()` value (0) and do not touch it.
    pub overdraft_limit: i64,
}

// Parent / child pair for the prefetch-inside-atomic test. The `_p4`
// suffix keeps these isolated from the Phase 3 prefetch fixtures.
#[model(table = "ledgers_p4")]
#[derive(Debug, Clone)]
pub struct Ledger {
    pub name: String,
}

#[model(table = "entries_p4", no_default)]
#[derive(Debug, Clone)]
pub struct Entry {
    pub ledger_id: ForeignKey<Ledger>,
    pub memo: String,
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

async fn setup_phase4(pool: &PgPool) {
    heeranjid_sqlx::install_schema(pool).await.unwrap();
    heeranjid_sqlx::seed_default_node(pool).await.unwrap();

    let db_name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query(&format!(
        "ALTER DATABASE \"{db_name}\" SET heer.node_id = '1'"
    ))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("SELECT set_heer_node_id(1)")
        .execute(pool)
        .await
        .unwrap();

    const ACCOUNTS_DDL: &str = include_str!("migrations/phase4/001_accounts.sql");
    const LEDGERS_DDL: &str = include_str!("migrations/phase4/002_ledgers.sql");
    const ENTRIES_DDL: &str = include_str!("migrations/phase4/003_entries.sql");

    sqlx::query(ACCOUNTS_DDL)
        .execute(pool)
        .await
        .expect("apply 001_accounts.sql");
    sqlx::query(LEDGERS_DDL)
        .execute(pool)
        .await
        .expect("apply 002_ledgers.sql");
    sqlx::query(ENTRIES_DDL)
        .execute(pool)
        .await
        .expect("apply 003_entries.sql");
}

fn entry_for_insert(memo: &str, ledger: &Ledger) -> Entry {
    Entry {
        id: ::djogi::types::__heerid_default(),
        created_at: ::djogi::types::DateTime::UNIX_EPOCH,
        updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
        ledger_id: ForeignKey::new(ledger.id),
        memo: memo.into(),
    }
}

// ---------------------------------------------------------------------------
// Task 1 integration tests — atomic() / savepoints / on_commit
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn atomic_commits_on_success(pool: PgPool) {
    setup_phase4(&pool).await;

    atomic(&pool, |ctx| {
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

    let mut verify_ctx = ::djogi::DjogiContext::from_pool(pool.clone());
    let count: i64 = Account::objects().count(&mut verify_ctx).await.unwrap();
    assert_eq!(count, 1, "committed row must be visible after the scope");
}

#[sqlx::test]
async fn atomic_rolls_back_on_err(pool: PgPool) {
    setup_phase4(&pool).await;

    let res = atomic(&pool, |ctx| {
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

    let mut verify_ctx = ::djogi::DjogiContext::from_pool(pool.clone());
    let count: i64 = Account::objects().count(&mut verify_ctx).await.unwrap();
    assert_eq!(count, 0, "rollback must leave no rows");
}

#[sqlx::test]
async fn nested_atomic_uses_savepoints(pool: PgPool) {
    setup_phase4(&pool).await;

    atomic(&pool, |outer| {
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

    let mut verify_ctx = ::djogi::DjogiContext::from_pool(pool.clone());
    let count: i64 = Account::objects().count(&mut verify_ctx).await.unwrap();
    assert_eq!(count, 1, "only the outer row survives the nested rollback");
}

#[sqlx::test]
async fn on_commit_fires_after_outer_commit(pool: PgPool) {
    setup_phase4(&pool).await;
    let flag = Arc::new(AtomicBool::new(false));

    {
        let flag = flag.clone();
        atomic(&pool, |ctx| {
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

#[sqlx::test]
async fn on_commit_does_not_fire_on_rollback(pool: PgPool) {
    setup_phase4(&pool).await;
    let flag = Arc::new(AtomicBool::new(false));

    let _res = {
        let flag = flag.clone();
        atomic(&pool, |ctx| {
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

#[sqlx::test]
async fn savepoint_rollback_discards_inner_on_commit(pool: PgPool) {
    setup_phase4(&pool).await;
    let count = Arc::new(AtomicUsize::new(0));

    {
        let count = count.clone();
        atomic(&pool, |outer| {
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

#[sqlx::test]
async fn nested_atomic_on_commit_promotes_to_outer(pool: PgPool) {
    setup_phase4(&pool).await;
    let count = Arc::new(AtomicUsize::new(0));

    {
        let count = count.clone();
        atomic(&pool, |outer| {
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
// retry_on_conflict — happy path + non-lock short-circuit. Actual
// lock-error retry semantics need a real concurrent scenario and are
// exercised in Task 7 (row locks).
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn retry_on_conflict_does_not_retry_on_success(pool: PgPool) {
    setup_phase4(&pool).await;
    let mut ctx = ::djogi::DjogiContext::from_pool(pool.clone());

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

#[sqlx::test]
async fn retry_on_conflict_short_circuits_on_non_lock_error(pool: PgPool) {
    setup_phase4(&pool).await;
    let mut ctx = ::djogi::DjogiContext::from_pool(pool.clone());

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

// ---------------------------------------------------------------------------
// save() rehydration — `UPDATE ... RETURNING *` mutates `self` with
// DB truth so triggers, server-side defaults, and the advanced
// `updated_at` all surface on the receiver. Task 2 scope.
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn save_rehydrates_updated_at(pool: PgPool) {
    setup_phase4(&pool).await;
    let mut ctx = ::djogi::DjogiContext::from_pool(pool.clone());

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

#[sqlx::test]
async fn save_reflects_trigger_modified_fields(pool: PgPool) {
    setup_phase4(&pool).await;
    let mut ctx = ::djogi::DjogiContext::from_pool(pool.clone());

    // BEFORE UPDATE trigger that bumps balance by 1 — verifies the
    // receiver sees the trigger-adjusted value after save.
    sqlx::query(
        "CREATE OR REPLACE FUNCTION accounts_trigger() RETURNS trigger AS $$ \
         BEGIN NEW.balance := NEW.balance + 1; RETURN NEW; END; \
         $$ LANGUAGE plpgsql;",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "CREATE TRIGGER t_accounts BEFORE UPDATE ON accounts \
         FOR EACH ROW EXECUTE FUNCTION accounts_trigger();",
    )
    .execute(&pool)
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
// Prefetch-in-atomic — proves `PrefetchLoaderFn` works over a
// transaction-backed context.
// ---------------------------------------------------------------------------

#[sqlx::test]
async fn prefetch_works_inside_atomic(pool: PgPool) {
    setup_phase4(&pool).await;

    // All writes + reads happen inside a single atomic scope. If the
    // prefetch loader still bailed on `ContextInner::Transaction`, this
    // test would fail at the `.fetch_all_prefetched(ctx)` call with a
    // `Sqlx(Configuration(...))` error. The assertion on the resolved
    // relation proves the loader ran over the transaction-backed
    // context and stitched the parent row correctly.
    atomic(&pool, |ctx| {
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

#[sqlx::test]
async fn field_vs_field_filter(pool: PgPool) {
    setup_phase4(&pool).await;

    // Seed + query inside a single `atomic()` scope. The Phase 2
    // integration pattern (`pool.begin()` + `SELECT set_heer_node_id(1)`)
    // is what makes multi-INSERT fixtures robust against pool connections
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
    atomic(&pool, |ctx| {
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
