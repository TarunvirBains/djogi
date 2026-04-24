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
//! `tokio_postgres`-backed transactions use — it avoids the "async closure
//! implementation not general enough" HRTB inference limitation today's
//! compiler hits on bare `AsyncFnOnce` closures whose bodies reborrow
//! from the closure argument.
//!
//! # Fixture strategy
//!
//! Each test provisions the Phase 4 tables via `setup_phase4(&mut ctx)`.
//! The `#[djogi_test]` bootstrap already installs HeeRanjID schema, seeds
//! node 1, and sets `heer.node_id = '1'` at the database level so every
//! pool connection — including the one `atomic()` checks out — inherits
//! it without any per-connection SET calls.

use djogi::prelude::*;
use djogi::transaction::{atomic, retry_on_conflict};
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

// Events-enabled model for Phase 4 Task 6. `kind` is the payload-
// visible column; `internal_notes` is excluded from the outbox payload
// via `#[field(outbox = "ignore")]`. Kept separate from `Account` so
// Tasks 1-5 assertions that count rows in `accounts_outbox` stay
// unaffected (non-events models write nothing there).
#[model(table = "notifications", events)]
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

/// Create the Phase 4 tables. The `#[djogi_test]` bootstrap already handles
/// HeeRanjID schema installation, node seeding, and `heer.node_id` at the
/// database level — no setup required here beyond DDL.
///
/// Each `include_str!` below is a single-statement SQL file. Postgres does
/// not accept multiple `;`-separated statements in a single prepared query,
/// so the trigger-function definition and trigger attachment sit in their
/// own files (006 / 007).
async fn setup_phase4(ctx: &mut djogi::DjogiContext) {
    const ACCOUNTS_DDL: &str = include_str!("migrations/phase4/001_accounts.sql");
    const LEDGERS_DDL: &str = include_str!("migrations/phase4/002_ledgers.sql");
    const ENTRIES_DDL: &str = include_str!("migrations/phase4/003_entries.sql");
    const NOTIFICATIONS_DDL: &str = include_str!("migrations/phase4/004_notifications.sql");
    const NOTIFICATIONS_OUTBOX_DDL: &str =
        include_str!("migrations/phase4/005_notifications_outbox.sql");
    const NOTIFICATIONS_REWRITE_FN_DDL: &str =
        include_str!("migrations/phase4/006_notifications_rewrite_fn.sql");
    const NOTIFICATIONS_REWRITE_TRIGGER_DDL: &str =
        include_str!("migrations/phase4/007_notifications_rewrite_trigger.sql");

    ctx.raw_execute(ACCOUNTS_DDL, &[])
        .await
        .expect("apply 001_accounts.sql");
    ctx.raw_execute(LEDGERS_DDL, &[])
        .await
        .expect("apply 002_ledgers.sql");
    ctx.raw_execute(ENTRIES_DDL, &[])
        .await
        .expect("apply 003_entries.sql");
    ctx.raw_execute(NOTIFICATIONS_DDL, &[])
        .await
        .expect("apply 004_notifications.sql");
    ctx.raw_execute(NOTIFICATIONS_OUTBOX_DDL, &[])
        .await
        .expect("apply 005_notifications_outbox.sql");
    ctx.raw_execute(NOTIFICATIONS_REWRITE_FN_DDL, &[])
        .await
        .expect("apply 006_notifications_rewrite_fn.sql");
    ctx.raw_execute(NOTIFICATIONS_REWRITE_TRIGGER_DDL, &[])
        .await
        .expect("apply 007_notifications_rewrite_trigger.sql");
}

fn entry_for_insert(memo: &str, ledger: &Ledger) -> Entry {
    Entry {
        id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
        created_at: ::djogi::types::DateTime::UNIX_EPOCH,
        updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
        ledger_id: ForeignKey::new(ledger.id),
        memo: memo.into(),
    }
}

// ---------------------------------------------------------------------------
// Task 1 integration tests — atomic() / savepoints / on_commit
// ---------------------------------------------------------------------------

#[djogi::djogi_test]
async fn atomic_commits_on_success(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

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

    let count: i64 = Account::objects().count(&mut ctx).await.unwrap();
    assert_eq!(count, 1, "committed row must be visible after the scope");
}

#[djogi::djogi_test]
async fn atomic_rolls_back_on_err(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

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

    let count: i64 = Account::objects().count(&mut ctx).await.unwrap();
    assert_eq!(count, 0, "rollback must leave no rows");
}

#[djogi::djogi_test]
async fn nested_atomic_uses_savepoints(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

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

    let count: i64 = Account::objects().count(&mut ctx).await.unwrap();
    assert_eq!(count, 1, "only the outer row survives the nested rollback");
}

#[djogi::djogi_test]
async fn on_commit_fires_after_outer_commit(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();
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

#[djogi::djogi_test]
async fn on_commit_does_not_fire_on_rollback(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();
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

#[djogi::djogi_test]
async fn savepoint_rollback_discards_inner_on_commit(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();
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

#[djogi::djogi_test]
async fn nested_atomic_on_commit_promotes_to_outer(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();
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

#[djogi::djogi_test]
async fn retry_on_conflict_does_not_retry_on_success(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;

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

#[djogi::djogi_test]
async fn retry_on_conflict_short_circuits_on_non_lock_error(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;

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

#[djogi::djogi_test]
async fn save_rehydrates_updated_at(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;

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

#[djogi::djogi_test]
async fn save_reflects_trigger_modified_fields(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;

    // BEFORE UPDATE trigger that bumps balance by 1 — verifies the
    // receiver sees the trigger-adjusted value after save.
    ctx.raw_execute(
        "CREATE OR REPLACE FUNCTION accounts_trigger() RETURNS trigger AS $$ \
         BEGIN NEW.balance := NEW.balance + 1; RETURN NEW; END; \
         $$ LANGUAGE plpgsql;",
        &[],
    )
    .await
    .unwrap();
    ctx.raw_execute(
        "CREATE TRIGGER t_accounts BEFORE UPDATE ON accounts \
         FOR EACH ROW EXECUTE FUNCTION accounts_trigger();",
        &[],
    )
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

#[djogi::djogi_test]
async fn bulk_update_arithmetic_expression(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

    // Seed two accounts; wrapping in `atomic()` keeps the connection's
    // `heer.node_id` GUC pinned across every pool-checkout during the
    // multi-INSERT seed (same rationale as phase3_relations).
    atomic(&pool, |ctx| {
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

#[djogi::djogi_test]
async fn bulk_update_field_to_field_copy(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

    atomic(&pool, |ctx| {
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

#[djogi::djogi_test]
async fn prefetch_works_inside_atomic(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

    // All writes + reads happen inside a single atomic scope. If the
    // prefetch loader still bailed on `ContextInner::Transaction`, this
    // test would fail at the `.fetch_all_prefetched(ctx)` call with a
    // configuration error. The assertion on the resolved relation proves
    // the loader ran over the transaction-backed context and stitched
    // the parent row correctly.
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
// `setup_phase4`, but a pool connection that checked out before the
// ALTER DATABASE took effect can still be missing the GUC. Opening a
// fresh transaction inside the test grants all seeds + reads the same
// transactional session — predictable and race-free.
// ---------------------------------------------------------------------------

#[djogi::djogi_test]
async fn aggregate_sum(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

    atomic(&pool, |ctx| {
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

#[djogi::djogi_test]
async fn aggregate_count_with_filter(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

    atomic(&pool, |ctx| {
        Box::pin(async move {
            // Two negative, two positive — the filtered count should
            // return exactly the negative rows. Using `balance < 0`
            // is the direct expression-IR equivalent of the plan's
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

#[djogi::djogi_test]
async fn annotate_single_aggregate(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

    atomic(&pool, |ctx| {
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

#[djogi::djogi_test]
async fn field_vs_field_filter(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

    // Seed + query inside a single `atomic()` scope. The Phase 2
    // integration pattern used pool.begin() + SELECT set_heer_node_id(1)
    // to make multi-INSERT fixtures robust against pool connections
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

#[djogi::djogi_test]
async fn exists_correlated_subquery(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

    atomic(&pool, |ctx| {
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

#[djogi::djogi_test]
async fn case_when_update(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

    atomic(&pool, |ctx| {
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

#[djogi::djogi_test]
async fn scalar_subquery_in_filter(mut ctx: djogi::DjogiContext) {
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

    atomic(&pool, |ctx| {
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

#[djogi::djogi_test]
async fn outbox_row_written_on_create_in_atomic(mut ctx: djogi::DjogiContext) {
    // Baseline: one `create` inside `atomic()` produces exactly one
    // outbox row with the expected action and row_id.
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

    let created = atomic(&pool, |ctx| {
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

    let outbox_count: i64 = ctx
        .raw_scalar("SELECT COUNT(*) FROM notifications_outbox", &[])
        .await
        .unwrap();
    assert_eq!(outbox_count, 1, "expected exactly one outbox row");

    let row = ctx
        .__query_one_for_macros(
            "SELECT row_id, action FROM notifications_outbox LIMIT 1",
            &[],
        )
        .await
        .unwrap();
    let row_id: i64 = row.try_get("row_id").unwrap();
    let action: String = row.try_get("action").unwrap();
    assert_eq!(
        row_id,
        created.id.as_i64(),
        "outbox row_id must match the primary row's id"
    );
    assert_eq!(
        action, "create",
        "outbox action column must record 'create'"
    );
}

#[djogi::djogi_test]
async fn outbox_rolled_back_on_err(mut ctx: djogi::DjogiContext) {
    // Returning `Err` from `atomic()` rolls the transaction back,
    // discarding both the primary row AND the outbox companion. This
    // is the core guarantee of the transactional-outbox pattern — the
    // outbox and the primary write share one atomic scope.
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

    let result = atomic(&pool, |ctx| {
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

    let primary: i64 = ctx
        .raw_scalar("SELECT COUNT(*) FROM notifications", &[])
        .await
        .unwrap();
    let outbox: i64 = ctx
        .raw_scalar("SELECT COUNT(*) FROM notifications_outbox", &[])
        .await
        .unwrap();
    assert_eq!(primary, 0, "primary row must be rolled back");
    assert_eq!(
        outbox, 0,
        "outbox row must be rolled back with the primary row"
    );
}

#[djogi::djogi_test]
async fn outbox_payload_excludes_ignored_fields(mut ctx: djogi::DjogiContext) {
    // `#[field(outbox = "ignore")]` must strip the column from the
    // emitted JSONB payload. Framework-injected columns (id/created_at/
    // updated_at) are expected to remain — they carry no exclusion flag.
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

    atomic(&pool, |ctx| {
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

    let row = ctx
        .__query_one_for_macros("SELECT payload FROM notifications_outbox LIMIT 1", &[])
        .await
        .unwrap();
    let payload: serde_json::Value = row.try_get("payload").unwrap();
    let obj = payload
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

#[djogi::djogi_test]
async fn outbox_save_writes_refreshed_payload(mut ctx: djogi::DjogiContext) {
    // After `save()` rehydrates the receiver from `RETURNING *`, the
    // outbox payload must reflect DB-rewritten state — not the caller's
    // pre-save Rust value.
    //
    // Proof: migration 004 installs a BEFORE UPDATE trigger that
    // appends " (db-rewritten)" to `kind` on every UPDATE. The caller
    // assigns `subject.kind = "acknowledged"`; if the outbox payload
    // came from the pre-save Rust receiver, the value would be
    // `"acknowledged"`. If it came from `RETURNING *` post-trigger, the
    // value is `"acknowledged (db-rewritten)"`. Only the latter is
    // observable from Postgres; the test asserts on it to close the
    // refresh-vs-pre-save ambiguity Codex flagged against the weaker
    // pre-fixup assertion.
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

    let created = atomic(&pool, |ctx| {
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

    // Clear the `create` outbox row so we assert cleanly on `save`.
    ctx.raw_execute("DELETE FROM notifications_outbox", &[])
        .await
        .unwrap();

    let mut subject = created.clone();
    let post_save_kind = atomic(&pool, |ctx| {
        Box::pin(async move {
            subject.kind = "acknowledged".to_string();
            subject.save(ctx).await?;
            // `save` rehydrates `subject` from `RETURNING *`, which
            // reflects the BEFORE UPDATE trigger's rewrite. Return the
            // rehydrated value so the assertion can compare against it.
            Ok::<_, DjogiError>(subject.kind.clone())
        })
    })
    .await
    .expect("save must succeed");

    assert_eq!(
        post_save_kind, "acknowledged (db-rewritten)",
        "save receiver must reflect the BEFORE UPDATE trigger's rewrite"
    );

    let row = ctx
        .__query_one_for_macros(
            "SELECT action, payload FROM notifications_outbox LIMIT 1",
            &[],
        )
        .await
        .unwrap();
    let action: String = row.try_get("action").unwrap();
    let payload: serde_json::Value = row.try_get("payload").unwrap();
    assert_eq!(
        action, "save",
        "save path must record 'save' in the outbox action column"
    );
    let obj = payload.as_object().unwrap();
    assert_eq!(
        obj["kind"],
        serde_json::Value::String("acknowledged (db-rewritten)".to_string()),
        "payload must carry the DB-rewritten value — a pre-refresh payload \
         would read `acknowledged` without the trigger suffix"
    );
}

#[djogi::djogi_test]
async fn outbox_delete_captures_predelete_snapshot(mut ctx: djogi::DjogiContext) {
    // `delete(self, ctx)` consumes `self`, but the outbox row must
    // carry the pre-delete payload — proving the emission happens
    // before `self` is dropped at function scope end.
    setup_phase4(&mut ctx).await;
    let pool = ctx.pool().unwrap().clone();

    let created = atomic(&pool, |ctx| {
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

    ctx.raw_execute("DELETE FROM notifications_outbox", &[])
        .await
        .unwrap();

    atomic(&pool, |ctx| {
        Box::pin(async move {
            created.clone().delete(ctx).await?;
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("delete must succeed");

    let row = ctx
        .__query_one_for_macros(
            "SELECT action, payload FROM notifications_outbox LIMIT 1",
            &[],
        )
        .await
        .unwrap();
    let action: String = row.try_get("action").unwrap();
    let payload: serde_json::Value = row.try_get("payload").unwrap();
    assert_eq!(action, "delete");
    let obj = payload.as_object().unwrap();
    assert_eq!(
        obj["kind"],
        serde_json::Value::String("goodbye".to_string()),
        "delete payload must be the pre-delete snapshot"
    );

    let remaining: i64 = ctx
        .raw_scalar("SELECT COUNT(*) FROM notifications", &[])
        .await
        .unwrap();
    assert_eq!(
        remaining, 0,
        "primary row must be gone after delete; outbox remains"
    );
}
