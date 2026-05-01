//! Phase 8-Zero Cluster A T6 — DjogiPool live integration tests.
//!
//! These tests need a reachable Postgres at `DATABASE_URL` because the
//! invariants under test are deadpool lifecycle behaviours that only
//! materialise once a real socket is open:
//!
//! - `post_connect` fires exactly once per physical connection and not
//!   on the per-checkout reuse path.
//! - `max_size(N)` actually saturates at N: an `(N+1)`-th concurrent
//!   checkout times out via the configured wait deadline and surfaces
//!   `DjogiError::PoolTimeout { phase: "wait" }`.
//! - `with_client` returns the connection to the pool on `Ok`, detaches
//!   on `Err`, and detaches on panic — proven by checking the deadpool
//!   `Status::size` counter before and after each scenario.
//! - `from_database_config` walks env > `[database].max_connections` >
//!   builder default and the chosen size lands as the pool's `max_size`.
//!
//! The unit-test layer in `djogi/src/pg/pool.rs` covers builder
//! type-system shape, `PoolTimeout` classification, `max_size(0)`
//! rejection, and the env-resolution chain — none of which need a live
//! Postgres. This file owns the rest.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use djogi::DjogiError;
use djogi::pg::pool::{DjogiPool, ENV_DATABASE_MAX_CONNECTIONS};
use djogi::testing::{TestDbCleanup, setup_test_db, teardown_test_db};

/// Helper: provision a per-test Postgres database via the standard
/// `#[djogi_test]` harness and return the per-test URL alongside the
/// cleanup token. The original `DjogiContext` (and its internal pool)
/// is dropped — the tests build their own pool against the same
/// per-test DB so they can exercise builder knobs without contention
/// with the harness's default pool.
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

// ---------------------------------------------------------------------------
// post_connect — fires once per physical connection
// ---------------------------------------------------------------------------

/// Open a pool with `max_size = 1` and a `post_connect` hook that
/// increments a counter. Run multiple sequential `with_client` calls.
/// Because `max_size = 1` constrains the pool to a single physical
/// connection, the same connection is reused across all checkouts and
/// the hook should fire **exactly once** — proving that `post_connect`
/// is not a per-checkout hook.
#[tokio::test]
async fn pool_post_connect_fires_once_per_physical_connection() {
    let (cleanup, url) = provision_test_db().await;
    let counter = Arc::new(AtomicUsize::new(0));

    let pool = {
        let counter = counter.clone();
        DjogiPool::builder(&url)
            .max_size(1)
            .post_connect(move |client| {
                let counter = counter.clone();
                Box::pin(async move {
                    // Real setup work — exercise the closure body so the
                    // test pins the round-trip from build → first
                    // checkout → hook → SQL.
                    client
                        .batch_execute("SET application_name = 'djogi-t6-test'")
                        .await
                        .map_err(djogi::DjogiError::from)?;
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            })
            .build()
            .await
            .expect("pool builds")
    };

    // Issue three sequential checkouts. With `max_size = 1` deadpool is
    // forced to reuse the same physical connection for all three.
    for _ in 0..3 {
        pool.with_client(|client| {
            Box::pin(async move {
                let _ = client
                    .query_one("SELECT 1", &[])
                    .await
                    .map_err(djogi::DjogiError::from)?;
                Ok::<_, DjogiError>(())
            })
        })
        .await
        .expect("checkout succeeds");
    }

    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "post_connect must fire exactly once per physical connection, \
         not on every checkout"
    );

    teardown_test_db(cleanup).await;
}

/// A `post_connect` hook that returns `Err` aborts the originating
/// `pool.get()` (or `with_client` checkout). The error surfaces as a
/// `DjogiError::Db` whose message carries the `post_connect:` prefix
/// added by the lowering, so adopters can grep server-side logs for
/// the failed-startup case.
#[tokio::test]
async fn pool_post_connect_error_aborts_checkout() {
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(1)
        .post_connect(|_client| {
            Box::pin(async {
                Err(DjogiError::Validation(
                    "intentional post_connect failure for the test".into(),
                ))
            })
        })
        .build()
        .await
        .expect("pool builds; physical connection happens lazily");

    let result = pool
        .with_client(|client| {
            Box::pin(async move {
                let _ = client;
                Ok::<_, DjogiError>(())
            })
        })
        .await;

    let err = result.expect_err("post_connect Err must abort the checkout");
    let msg = format!("{err}");
    assert!(
        msg.contains("post_connect"),
        "error must carry the post_connect lowering prefix; got: {msg}"
    );

    teardown_test_db(cleanup).await;
}

// ---------------------------------------------------------------------------
// max_size + timeout — saturation surfaces PoolTimeout
// ---------------------------------------------------------------------------

/// `.max_size(2)` blocks an attempted third concurrent checkout; with
/// `.timeout` set, the third request returns
/// `DjogiError::PoolTimeout { phase: "wait" }` after the deadline.
///
/// This is the v3 spec's PR-exit assertion ("`max_size(2)` blocks third
/// concurrent acquire" + "Timeout error mapping to `DjogiError::PoolTimeout`").
#[tokio::test]
async fn pool_max_size_saturation_surfaces_pool_timeout() {
    let (cleanup, url) = provision_test_db().await;

    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .timeout(Duration::from_millis(150))
        .build()
        .await
        .expect("pool builds");

    // Hold two long-running checkouts so the pool is saturated. Each
    // closure parks its connection on a `pg_sleep` for far longer than
    // the wait timeout — the third checkout must fail before they
    // finish.
    let p1 = pool.clone();
    let p2 = pool.clone();
    let hold_a = tokio::spawn(async move {
        p1.with_client(|client| {
            Box::pin(async move {
                let _ = client
                    .query_one("SELECT pg_sleep(0.5)", &[])
                    .await
                    .map_err(djogi::DjogiError::from)?;
                Ok::<_, DjogiError>(())
            })
        })
        .await
    });
    let hold_b = tokio::spawn(async move {
        p2.with_client(|client| {
            Box::pin(async move {
                let _ = client
                    .query_one("SELECT pg_sleep(0.5)", &[])
                    .await
                    .map_err(djogi::DjogiError::from)?;
                Ok::<_, DjogiError>(())
            })
        })
        .await
    });

    // Yield once so the two holders enter their pg_sleep before we try
    // the saturating third checkout.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let third = pool
        .with_client(|client| {
            Box::pin(async move {
                let _ = client;
                Ok::<_, DjogiError>(())
            })
        })
        .await;

    let err = third.expect_err("third concurrent checkout must time out");
    match err {
        DjogiError::PoolTimeout { phase, .. } => assert_eq!(phase, "wait"),
        other => panic!("expected PoolTimeout(wait), got: {other:?}"),
    }

    // Drain holders before tearing down so the cleanup path has live
    // connections to recycle.
    let _ = hold_a.await.expect("join holder a");
    let _ = hold_b.await.expect("join holder b");

    teardown_test_db(cleanup).await;
}

// ---------------------------------------------------------------------------
// with_client — clean exit returns, dirty exit detaches
// ---------------------------------------------------------------------------

/// On a clean exit the connection returns to the pool — the size after
/// the call equals 1 (one physical connection, currently idle).
#[tokio::test]
async fn pool_with_client_returns_connection_on_ok() {
    let (cleanup, url) = provision_test_db().await;
    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    pool.with_client(|client| {
        Box::pin(async move {
            let _ = client
                .query_one("SELECT 1", &[])
                .await
                .map_err(djogi::DjogiError::from)?;
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("clean exit");

    let status = pool.status();
    assert_eq!(status.available, 1, "one connection should be idle");
    assert_eq!(status.size, 1, "exactly one physical connection in pool");

    teardown_test_db(cleanup).await;
}

/// On `Err` the guard detaches: the deadpool `Object::take` path fires,
/// the underlying client is dropped, and the pool's `size` counter
/// returns to zero. The next checkout creates a fresh connection.
#[tokio::test]
async fn pool_with_client_detaches_on_err() {
    let (cleanup, url) = provision_test_db().await;
    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    let result: Result<(), DjogiError> = pool
        .with_client(|client| {
            Box::pin(async move {
                let _ = client
                    .query_one("SELECT 1", &[])
                    .await
                    .map_err(djogi::DjogiError::from)?;
                // Return Err — the guard must detach.
                Err(DjogiError::Validation("intentional dirty exit".into()))
            })
        })
        .await;

    assert!(result.is_err(), "closure returned Err");

    let status = pool.status();
    assert_eq!(
        status.size, 0,
        "Err path must detach the Object so the physical connection is closed; \
         pool.size should drop back to 0, got: {status:?}"
    );

    // Next checkout creates a fresh connection — this is the
    // recovery-path assertion. If detach were broken we'd hand back a
    // poisoned client.
    pool.with_client(|client| {
        Box::pin(async move {
            let _ = client
                .query_one("SELECT 1", &[])
                .await
                .map_err(djogi::DjogiError::from)?;
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("recovery checkout opens a fresh physical connection");

    teardown_test_db(cleanup).await;
}

/// On panic the unwind drops the guard while `committed = false`, which
/// detaches via `Object::take`. We use `tokio::spawn` + `JoinError` to
/// catch the panic without aborting the test process.
#[tokio::test]
async fn pool_with_client_detaches_on_panic() {
    let (cleanup, url) = provision_test_db().await;
    let pool = DjogiPool::builder(&url)
        .max_size(2)
        .build()
        .await
        .expect("pool builds");

    let p = pool.clone();
    let join = tokio::spawn(async move {
        p.with_client(|client| {
            Box::pin(async move {
                let _ = client;
                panic!("intentional panic inside with_client closure");
                #[allow(unreachable_code)]
                Ok::<(), DjogiError>(())
            })
        })
        .await
    });

    let join_err = join.await.expect_err("task must panic");
    assert!(
        join_err.is_panic(),
        "the spawned task must report a panic, got: {join_err:?}"
    );

    let status = pool.status();
    assert_eq!(
        status.size, 0,
        "panic path must detach the Object so the physical connection is closed; \
         pool.size should drop back to 0, got: {status:?}"
    );

    teardown_test_db(cleanup).await;
}

// ---------------------------------------------------------------------------
// from_database_config — wiring through the resolution chain
// ---------------------------------------------------------------------------

/// `from_database_config` honours `[database].max_connections` when env
/// is unset.
#[tokio::test]
async fn from_database_config_honours_toml_max_connections() {
    let (cleanup, url) = provision_test_db().await;
    // Make sure a stale env variable from another test doesn't bleed
    // into this one.
    // Safety: process-global mutation; lib tests run single-threaded
    // under `--test-threads=1`.
    unsafe { std::env::remove_var(ENV_DATABASE_MAX_CONNECTIONS) };

    let cfg = djogi::config::DatabaseConfig {
        url: url.clone(),
        max_connections: 13,
        dev_mode: false,
    };
    let pool = DjogiPool::from_database_config(&cfg)
        .await
        .expect("from_database_config builds");

    assert_eq!(
        pool.status().max_size,
        13,
        "[database].max_connections from config must reach the pool"
    );

    teardown_test_db(cleanup).await;
}

/// `DJOGI_DATABASE_MAX_CONNECTIONS` overrides the TOML field — env wins
/// in the resolution chain.
#[tokio::test]
async fn from_database_config_env_overrides_toml() {
    let (cleanup, url) = provision_test_db().await;
    // Safety: process-global mutation; lib tests run single-threaded.
    unsafe { std::env::set_var(ENV_DATABASE_MAX_CONNECTIONS, "21") };

    let cfg = djogi::config::DatabaseConfig {
        url: url.clone(),
        max_connections: 13,
        dev_mode: false,
    };
    let pool = DjogiPool::from_database_config(&cfg)
        .await
        .expect("from_database_config builds");

    assert_eq!(
        pool.status().max_size,
        21,
        "env var must beat [database].max_connections in resolution"
    );

    // Clean up so subsequent tests in the same process don't see this
    // value.
    unsafe { std::env::remove_var(ENV_DATABASE_MAX_CONNECTIONS) };

    teardown_test_db(cleanup).await;
}

// ---------------------------------------------------------------------------
// Public-status accessor used by the assertions above
// ---------------------------------------------------------------------------

/// `DjogiPool::status` must surface the deadpool counters so tests can
/// assert pool-state invariants. Without it the `with_client` lifecycle
/// tests above could not distinguish detach from return-to-pool.
#[tokio::test]
async fn pool_status_exposes_deadpool_counters() {
    let (cleanup, url) = provision_test_db().await;
    let pool = DjogiPool::builder(&url)
        .max_size(4)
        .build()
        .await
        .expect("pool builds");

    let status = pool.status();
    assert_eq!(status.max_size, 4);
    // No checkouts yet — the pool is empty, not pre-filled.
    assert_eq!(status.size, 0);

    teardown_test_db(cleanup).await;
}
