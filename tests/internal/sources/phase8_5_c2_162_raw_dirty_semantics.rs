// djogi#162 — pool-path raw SQL escape hatches must detach the connection
// from the pool on Err / panic / cancellation, mirroring
// `DjogiPool::with_client`'s `WithClientGuard`.
//
// Without the guard, a bypassed `raw_execute("SET ROLE ...")` (or any
// other session-state mutation) that errors mid-statement would leave a
// poisoned client in the pool — `RecyclingMethod::Fast` only checks
// `is_closed()` on return; it does NOT issue `ROLLBACK` / `RESET ALL` /
// `DISCARD ALL`. Each path below proves the new `PoolConnGuard` fires
// `PgConnection::detach()` on the dirty exit so the pool's `size`
// counter falls back to zero and the next checkout opens a fresh
// connection.
//
// The clean-exit assertion pins the happy path so a future refactor
// that breaks the guard's `committed = true` flip (turning every
// checkout into a detach) is also caught here.

use std::sync::OnceLock;
use std::time::Duration;

use djogi::DjogiError;
use djogi::pg::pool::DjogiPool;
use djogi::testing::{TestDbCleanup, setup_test_db, teardown_test_db};
use djogi::transaction::atomic;
use tokio::sync::{Mutex, MutexGuard};
// `RawAccessExt` is brought into scope by the
// `#[djogi::deliberately_bypass_convention_with_raw_sql]` attribute on the
// wrapper module that `include!`s this source.

/// Process-global async lock. Each test owns its own per-test database
/// and pool, but `setup_test_db`'s `CREATE DATABASE` shares the maintenance
/// connection with other internal live tests — serialising avoids
/// spurious contention failures on slow CI hosts.
async fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().await
}

async fn provision_test_db() -> (TestDbCleanup, String) {
    let (cleanup, ctx) = setup_test_db()
        .await
        .expect("setup_test_db must succeed against DATABASE_URL");
    let url = cleanup
        .test_url()
        .expect("cleanup token should yield a per-test URL");
    drop(ctx);
    (cleanup, url)
}

/// Clean exit: `raw_execute` returns `Ok` and the connection returns to
/// the pool the normal way. After the call the pool holds exactly one
/// physical connection, currently idle. This is the happy-path pin that
/// catches a future refactor turning every checkout into an unconditional
/// detach.
#[tokio::test]
async fn pool_raw_execute_returns_connection_on_ok() {
    let _lock = test_lock().await;
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    let mut ctx = djogi::DjogiContext::from_pool(pool.clone());
    let affected = clean_raw_execute(&mut ctx)
        .await
        .expect("clean exit returns Ok");
    assert_eq!(affected, 0, "DDL via raw_execute reports zero affected rows");

    let status = pool.status();
    assert_eq!(
        status.size, 1,
        "clean exit must return the connection to the pool; \
         pool.size should be 1, got: {status:?}"
    );
    assert_eq!(
        status.available, 1,
        "the returned connection should be idle; got: {status:?}"
    );

    teardown_test_db(cleanup).await;
}

/// Dirty exit (`Err`): `raw_execute` against malformed SQL surfaces a
/// `DjogiError::Db`; the new `PoolConnGuard` must observe `Err` and
/// detach the connection rather than recycle it. The pool's `size`
/// drops back to zero and the next checkout opens a fresh physical
/// connection.
#[tokio::test]
async fn pool_raw_execute_detaches_on_err() {
    let _lock = test_lock().await;
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    let mut ctx = djogi::DjogiContext::from_pool(pool.clone());
    let result = ctx
        .raw_execute(
            "DO $$ BEGIN RAISE EXCEPTION 'djogi#162 dirty exit sentinel'; END $$",
            &[],
        )
        .await;
    assert!(
        result.is_err(),
        "raw_execute against a RAISE EXCEPTION block must surface Err"
    );

    let status = pool.status();
    assert_eq!(
        status.size, 0,
        "Err path must detach the PgConnection so the physical \
         connection is closed; pool.size should drop back to 0, \
         got: {status:?}"
    );

    // Recovery checkout: a fresh physical connection must open without
    // inheriting the failed statement's state. If detach were broken
    // we'd hand back a client whose error context still applies.
    let again = clean_raw_execute(&mut ctx)
        .await
        .expect("recovery checkout should open a fresh connection");
    assert_eq!(again, 0);

    teardown_test_db(cleanup).await;
}

/// Dirty exit (cancellation): wrap the raw call in `tokio::time::timeout`
/// with a deadline so short the future is dropped before the statement
/// completes. Even though no `Err`/panic surfaces from the call site
/// (the timeout consumes the future), the `PoolConnGuard`'s `Drop` runs
/// while `committed = false` and detaches the connection. After the
/// timeout fires the pool's `size` falls back to zero, proving the
/// guard handles future cancellation as a dirty exit.
#[tokio::test]
async fn pool_raw_execute_detaches_on_cancellation() {
    let _lock = test_lock().await;
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    let mut ctx = djogi::DjogiContext::from_pool(pool.clone());

    // pg_sleep blocks server-side; the await-point inside raw_execute
    // is the natural cancellation seam. The 50ms timeout is comfortably
    // shorter than the requested sleep.
    let result = tokio::time::timeout(
        Duration::from_millis(50),
        ctx.raw_execute("SELECT pg_sleep(5)", &[]),
    )
    .await;
    assert!(
        result.is_err(),
        "the 50ms timeout must fire before pg_sleep(5) returns"
    );

    let status = pool.status();
    assert_eq!(
        status.size, 0,
        "cancellation must detach the PgConnection so the physical \
         connection is closed; pool.size should drop back to 0, \
         got: {status:?}"
    );

    teardown_test_db(cleanup).await;
}

/// Helper: a raw_execute that always succeeds. `CREATE TEMP TABLE … IF NOT
/// EXISTS` is idempotent across both the happy-path test and the recovery
/// branch of the Err test (where the same context issues another raw_execute
/// after the dirty exit detaches the original connection).
async fn clean_raw_execute(ctx: &mut djogi::DjogiContext) -> Result<u64, DjogiError> {
    ctx.raw_execute(
        "CREATE TEMP TABLE IF NOT EXISTS djogi_162_clean_pin (value integer)",
        &[],
    )
    .await
}

/// Dirty exit via **post-query decode failure**. The SQL succeeds
/// server-side and mutates session state — `set_config(name, value,
/// false)` is the session-level form (equivalent to plain `SET`), which
/// survives a clean `COMMIT` and rides the connection back to the pool.
/// (Postgres `ROLLBACK` would still revert a session-level `SET` issued
/// inside the same aborted transaction; this test runs the call on a
/// pool-checkout autocommit path, so neither commit nor rollback
/// applies — the GUC just sticks until the connection is closed.) The
/// framework-side `try_get_scalar::<i32>` decode then fails because the
/// returned column is `text`. Without `query_opt_with` routing the
/// decode through `PoolConnGuard`'s lifetime, the connection would
/// recycle back to the pool already armed for clean return — silently
/// handing the next checkout a session whose `application_name` GUC was
/// `djogi_162_post_decode_dirty`.
///
/// With the post-decode guard wiring, the decode `Err` flips the guard
/// to dirty and `PgConnection::detach` runs, dropping the connection's
/// underlying socket. `pool.status().size` falls back to zero, the same
/// invariant the SQL-level Err and cancellation tests pin.
#[tokio::test]
async fn pool_raw_scalar_detaches_on_post_query_decode_err() {
    let _lock = test_lock().await;
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    let mut ctx = djogi::DjogiContext::from_pool(pool.clone());
    let result: Result<i32, DjogiError> = ctx
        .raw_scalar(
            "SELECT set_config('application_name', 'djogi_162_post_decode_dirty', false)",
            &[],
        )
        .await;
    assert!(
        result.is_err(),
        "raw_scalar::<i32> against a `text`-returning function must surface Err"
    );

    let status = pool.status();
    assert_eq!(
        status.size, 0,
        "post-query decode failure must detach the PgConnection so the \
         physical connection is closed; pool.size should drop back to 0, \
         got: {status:?}"
    );

    teardown_test_db(cleanup).await;
}

/// Pool-backed raw SQL keeps the pre-#282 contract: session-scoped statements
/// still execute on the clean path. The new refusal only applies to
/// transaction-backed contexts where `atomic()` would otherwise invite callers
/// to assume rollback can scrub session state.
#[tokio::test]
async fn pool_raw_execute_still_allows_session_scoped_set() {
    let _lock = test_lock().await;
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    let mut ctx = djogi::DjogiContext::from_pool(pool);
    ctx.raw_execute("SET application_name = 'djogi_282_pool_allowed'", &[])
        .await
        .expect("pool-backed raw_execute should preserve the existing session-statement contract");

    teardown_test_db(cleanup).await;
}

/// Transaction-backed raw SQL must now reject plain session-level `SET`
/// even when the SQL is preceded by comments or mixed-case keywords.
#[tokio::test]
async fn transaction_raw_execute_rejects_plain_set_after_comments() {
    let _lock = test_lock().await;
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    let mut ctx = djogi::DjogiContext::from_pool(pool);
    atomic(&mut ctx, |tx| {
        Box::pin(async move {
            let err = tx
                .raw_execute(
                    "/* leading comment ; */ -- line comment\n sEt search_path = public",
                    &[],
                )
                .await
                .expect_err("plain SET inside atomic() must be refused before SQL reaches Postgres");
            match err {
                DjogiError::SessionStatementDisallowedInTransaction { statement, .. } => {
                    assert_eq!(statement, "SET");
                }
                other => panic!("expected SessionStatementDisallowedInTransaction(SET), got: {other:?}"),
            }
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("classifier refusal should not poison the outer transaction");

    teardown_test_db(cleanup).await;
}

/// `SET LOCAL`, `SET CONSTRAINTS`, and `SET TRANSACTION` are the explicit
/// allow-list under #282. They are transaction-scoped and therefore safe to
/// execute inside `atomic()`.
#[tokio::test]
async fn transaction_raw_execute_allows_transaction_scoped_set_forms() {
    let _lock = test_lock().await;
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    let mut ctx = djogi::DjogiContext::from_pool(pool);

    atomic(&mut ctx, |tx| {
        Box::pin(async move {
            tx.raw_execute("SET LOCAL statement_timeout = '5s'", &[])
                .await
                .expect("SET LOCAL must remain allowed");
            tx.raw_execute("SET CONSTRAINTS ALL IMMEDIATE", &[])
                .await
                .expect("SET CONSTRAINTS must remain allowed");
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("transaction-scoped SET forms must succeed");

    atomic(&mut ctx, |tx| {
        Box::pin(async move {
            tx.raw_execute("SET TRANSACTION ISOLATION LEVEL READ COMMITTED", &[])
                .await
                .expect("SET TRANSACTION must remain allowed when it is the first statement");
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("SET TRANSACTION must succeed in a fresh transaction");

    teardown_test_db(cleanup).await;
}

/// `raw_ddl` may contain multiple top-level statements, comments, quoted
/// strings, and dollar-quoted bodies. The #282 scanner must inspect each real
/// statement without naively splitting on every `;`.
#[tokio::test]
async fn transaction_raw_ddl_rejects_session_statement_after_dollar_quoted_body() {
    let _lock = test_lock().await;
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    let mut ctx = djogi::DjogiContext::from_pool(pool);
    atomic(&mut ctx, |tx| {
        Box::pin(async move {
            let err = tx
                .raw_ddl(
                    r#"
                        DO $body$
                        BEGIN
                            PERFORM '; still inside the body';
                            PERFORM $$nested ; dollar quote$$;
                        END
                        $body$;
                        /* the next top-level statement is the one that matters */
                        SET search_path = public;
                    "#,
                )
                .await
                .expect_err("raw_ddl must reject session-scoped statements inside atomic()");
            match err {
                DjogiError::SessionStatementDisallowedInTransaction { statement, .. } => {
                    assert_eq!(statement, "SET");
                }
                other => panic!("expected SessionStatementDisallowedInTransaction(SET), got: {other:?}"),
            }
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("preflight refusal should not poison the outer transaction");

    teardown_test_db(cleanup).await;
}

/// A safe `raw_ddl` batch with internal semicolons in a dollar-quoted body
/// must still execute. This is the positive pin against a naive `split(';')`
/// classifier.
#[tokio::test]
async fn transaction_raw_ddl_allows_safe_batch_with_dollar_quoted_body() {
    let _lock = test_lock().await;
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    let mut ctx = djogi::DjogiContext::from_pool(pool);
    atomic(&mut ctx, |tx| {
        Box::pin(async move {
            tx.raw_ddl(
                r#"
                    DO $body$
                    BEGIN
                        PERFORM '; safe body';
                    END
                    $body$;
                    CREATE TEMP TABLE djogi_282_safe_batch (value integer NOT NULL);
                "#,
            )
            .await
            .expect("safe raw_ddl batch should execute inside atomic()");
            tx.raw_execute("INSERT INTO djogi_282_safe_batch (value) VALUES (1)", &[])
                .await
                .expect("table created by safe batch should be usable");
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("safe raw_ddl batch must commit");

    teardown_test_db(cleanup).await;
}

// ============================================================================
// djogi#306 — transaction-control statement refusal tests.
// ============================================================================

/// #306 — COMMIT hidden behind comments and whitespace must still be
/// refused by the transaction-control classifier.
#[tokio::test]
async fn transaction_raw_execute_refuses_commit_after_leading_comments() {
    let _lock = test_lock().await;
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    let mut ctx = djogi::DjogiContext::from_pool(pool);

    atomic(&mut ctx, |tx| {
        Box::pin(async move {
            let err = tx.raw_execute(
                "  -- committing the transaction\nCOMMIT",
                &[],
            )
            .await
            .expect_err("raw_execute inside atomic() must refuse COMMIT after comments");
            match err {
                DjogiError::RawTransactionControlDisallowedInTransaction { statement, .. } => {
                    assert_eq!(statement, "COMMIT");
                }
                other => panic!("expected RawTransactionControlDisallowedInTransaction(COMMIT), got: {other:?}"),
            }
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("transaction-control refusal should not poison the outer transaction");

    teardown_test_db(cleanup).await;
}

/// #306 — Transaction-control alias forms (START TRANSACTION, END, ABORT)
/// must be refused with the correct statement label.
#[tokio::test]
async fn transaction_raw_execute_refuses_start_tx_end_abort_aliases() {
    let _lock = test_lock().await;
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    let mut ctx = djogi::DjogiContext::from_pool(pool);

    for (sql, expected_label) in [
        ("START TRANSACTION", "START TRANSACTION"),
        ("END", "END"),
        ("ABORT", "ABORT"),
    ] {
        let sql = sql;
        let expected_label = expected_label;
        atomic(&mut ctx, |tx| {
            Box::pin(async move {
                let err = tx.raw_execute(sql, &[])
                    .await
                    .expect_err("raw_execute inside atomic() must refuse transaction-control");
                match err {
                    DjogiError::RawTransactionControlDisallowedInTransaction { statement, .. } => {
                        assert_eq!(statement, expected_label);
                    }
                    other => panic!("expected RawTransactionControlDisallowedInTransaction({expected_label}), got: {other:?}"),
                }
                Ok::<_, DjogiError>(())
            })
        })
        .await
        .expect("transaction-control refusal should not poison the outer transaction");
    }

    teardown_test_db(cleanup).await;
}

/// #306 — SAVEPOINT refusal must not poison the transaction context,
/// allowing subsequent raw calls to succeed in the same atomic() block.
#[tokio::test]
async fn transaction_raw_execute_refuses_savepoint_without_poisoning() {
    let _lock = test_lock().await;
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    let mut ctx = djogi::DjogiContext::from_pool(pool);

    atomic(&mut ctx, |tx| {
        Box::pin(async move {
            // SAVEPOINT is refused
            let err = tx.raw_execute("SAVEPOINT my_sp", &[])
                .await
                .expect_err("SAVEPOINT must be refused inside atomic()");
            match err {
                DjogiError::RawTransactionControlDisallowedInTransaction { statement, .. } => {
                    assert_eq!(statement, "SAVEPOINT");
                }
                other => panic!("expected RawTransactionControlDisallowedInTransaction(SAVEPOINT), got: {other:?}"),
            }

            // Subsequent safe call must still work — context is not poisoned
            tx.raw_execute(
                "CREATE TEMP TABLE IF NOT EXISTS djogi_306_savepoint_test (id integer)",
                &[],
            )
            .await
            .expect("safe raw_execute must succeed after SAVEPOINT refusal");

            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("SAVEPOINT refusal must not poison the outer transaction");

    teardown_test_db(cleanup).await;
}

/// #306 REQ-306-5 — Poisoned context must return TransactionPoisoned
/// before any raw-SQL refusal check, preserving poison precedence.
#[tokio::test]
async fn transaction_raw_execute_returns_poison_before_transaction_control_refusal() {
    let _lock = test_lock().await;
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    let mut ctx = djogi::DjogiContext::from_pool(pool);

    // Poison the outer transaction by dropping a nested atomic() via timeout.
    // The NestedAtomicCancellationGuard fires on drop and calls
    // poison_transaction(NESTED_ATOMIC_CANCELLED_POISON_REASON).
    let _outer = atomic(&mut ctx, |tx| {
        Box::pin(async move {
            let _nested_result = tokio::time::timeout(
                Duration::from_millis(10),
                atomic(&mut *tx, |_inner_tx| {
                    Box::pin(async move {
                        // Intentionally sleep past the timeout so the nested
                        // atomic future is dropped, triggering the cancel guard.
                        tokio::time::sleep(Duration::from_secs(999)).await;
                        Ok::<_, DjogiError>(())
                    })
                }),
            )
            .await;
            // _nested_result is Err (timeout) — parent tx is now poisoned

            // Attempt a transaction-control statement — should get poison error,
            // not the classifier refusal. Poison takes precedence over classifier
            // per reject_transaction_backed_sql() ordering (poison check first).
            let err = (&mut *tx).raw_execute("COMMIT", &[])
                .await
                .expect_err("raw_execute on poisoned context must fail");
            match err {
                DjogiError::TransactionPoisoned { .. } => {
                    // Expected — poison takes precedence over classifier
                }
                other => panic!("expected TransactionPoisoned, got: {other:?}"),
            }

            Ok::<_, DjogiError>(())
        })
    })
    .await;
    // The outer atomic() itself may return Err(TransactionPoisoned) because
    // we poisoned it — that's expected behavior.

    teardown_test_db(cleanup).await;
}

/// #306 — Dollar-quoted bodies containing "COMMIT" are safe (not scanned),
/// but a real top-level COMMIT after the body must be refused.
#[tokio::test]
async fn transaction_raw_ddl_refuses_commit_after_dollar_quoted_body() {
    let _lock = test_lock().await;
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    let mut ctx = djogi::DjogiContext::from_pool(pool);

    atomic(&mut ctx, |tx| {
        Box::pin(async move {
            let err = tx.raw_ddl(
                r#"
                    DO $body$
                    BEGIN
                        PERFORM 'COMMIT';  -- safe: inside dollar quote
                    END
                    $body$;
                    COMMIT;  -- top-level: must be refused
                "#,
            )
            .await
            .expect_err("raw_ddl inside atomic() must refuse top-level COMMIT after dollar-quoted body");
            match err {
                DjogiError::RawTransactionControlDisallowedInTransaction { statement, .. } => {
                    assert_eq!(statement, "COMMIT");
                }
                other => panic!("expected RawTransactionControlDisallowedInTransaction(COMMIT), got: {other:?}"),
            }
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("transaction-control refusal should not poison the outer transaction");

    teardown_test_db(cleanup).await;
}

/// #306 REQ-306-7 — Pool-backed raw SQL (outside atomic()) is not guarded;
/// manual transaction control via raw_ddl must remain available.
#[tokio::test]
async fn pool_raw_ddl_still_allows_manual_transaction_control() {
    let _lock = test_lock().await;
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    // Pool-backed context (not inside atomic()) — no transaction guard
    let mut ctx = djogi::DjogiContext::from_pool(pool);

    ctx.raw_ddl(
        "BEGIN; \
         CREATE TABLE djogi_306_pool_manual_tx (id integer); \
         COMMIT;",
    )
    .await
    .expect("pool-backed raw_ddl with manual transaction control must succeed");

    // Verify the table was actually created by querying system catalog
    let rows = ctx.raw_rows(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'djogi_306_pool_manual_tx')",
        &[],
    )
    .await
    .expect("query for pool-created table must succeed");

    assert_eq!(rows.len(), 1);
    let table_exists: bool = rows[0].try_get(0)
        .expect("boolean column should decode from row");
    assert!(table_exists, "djogi_306_pool_manual_tx table should exist after raw_ddl with manual transaction control");

    // Clean up the test table to avoid polluting other tests
    ctx.raw_execute("DROP TABLE djogi_306_pool_manual_tx", &[])
        .await
        .ok();

    teardown_test_db(cleanup).await;
}
