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
use tokio::sync::{Mutex, MutexGuard};
// `RawAccessExt` is brought into scope by the
// `#[djogi::deliberately_bypass_convention_with_raw_sql]` attribute that
// decorates the wrapper module in `tests/internal/phase8_5_c2_162_raw_dirty_semantics.rs`.

/// Process-global async lock. The detach assertions below read
/// `pool.status().size` immediately after the guarded call returns —
/// running multiple of these in parallel against the same per-test
/// database is fine (each test owns its own pool), but provisioning the
/// per-test database under `setup_test_db` is the same code path as
/// other internal live tests, so we keep the test serialised by
/// convention to avoid surprising failures from CREATE DATABASE
/// contention on slow CI hosts.
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
/// server-side and mutates session state (the `set_config(name, value,
/// false)` form persists the new GUC across commit/rollback), then the
/// framework-side `try_get_scalar::<i32>` decode fails because the
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
