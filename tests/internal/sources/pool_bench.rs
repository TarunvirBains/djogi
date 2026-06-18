// DjogiPool smoke benchmarks.
//
// These are *not* perf guarantees. They are smoke bounds that protect
// three perf-claim areas:
//
// 1. `max_size` actually delivers concurrency — at least one widened
//    pool condition is faster wall-clock than running the same workload
//    against a 1-slot pool.
// 2. `post_connect` is not catastrophically expensive — a one-round-trip
//    callback during physical-connection creation stays within 3× of
//    no-callback startup. The 3× bound is loose on purpose; networking
//    dominates and we only want to catch the "we accidentally made it
//    100× slower" regression class.
// 3. `with_client`'s acquire+release-per-op cost is bounded relative
//    to holding one client across N ops. The dirty-by-default RAII
//    guard carries a per-op cost; this bench prints the ratio so it's
//    grep-able from CI logs.
//
// ## Running
//
// ```bash
// cargo test --test pool_bench -p djogi --all-features \
//     --release -- --test-threads=1 --nocapture
// ```
//
// Debug-mode runs also pass — the smoke bounds are loose enough — but
// the printed numbers are not perf claims. The banner at the top of
// each test reminds the reader.
//
// ## Why `tests/`, not `benches/`
//
// Cargo's `[[bench]]` harness pulls in nightly criterion-style infra
// we don't want for a v0.1.0 smoke check. Stuffing the timing logic
// into ordinary `#[tokio::test]` bodies keeps the test surface
// single-tracked and uses the same Postgres-reachability discipline
// as the rest of the integration suite.

use std::sync::Arc;
use std::time::{Duration, Instant};

use djogi::DjogiError;
use djogi::pg::pool::DjogiPool;
use djogi::testing::{TestDbCleanup, setup_test_db, teardown_test_db};
use futures::future::join_all;

/// Provision a per-test Postgres database via the standard `#[djogi_test]`
/// harness and return the per-test URL alongside the cleanup token. The
/// harness's own `DjogiContext` (and its internal pool) is dropped — the
/// benches build their own pools with custom knobs against the same
/// per-test DB.
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

/// Pretty-print the standard "this is a smoke bench, not a perf claim"
/// banner so anyone reading captured `--nocapture` output knows what
/// the numbers mean.
fn banner(name: &str) {
    let mode = if cfg!(debug_assertions) {
        "debug (numbers are NOT perf claims; smoke bounds only)"
    } else {
        "release (smoke bounds; not perf guarantees)"
    };
    println!();
    println!("=== {name} ===");
    println!("mode: {mode}");
}

/// Iteration count knob: fewer ops in debug builds so the suite stays
/// snappy when run inadvertently in `cargo test` defaults; release runs
/// get the full count for steadier numbers.
const fn iter_scale(release: usize, debug: usize) -> usize {
    if cfg!(debug_assertions) {
        debug
    } else {
        release
    }
}

// ---------------------------------------------------------------------------
// 1. max_size scaling
// ---------------------------------------------------------------------------

/// Verifies the pool actually delivers concurrency. We pin a fixed
/// total-op budget (`TOTAL_OPS`) and distribute it across N workers
/// where N matches `max_size`. With genuine concurrency, larger N
/// completes the same total workload in less wall-clock time, because
/// up to N `SELECT 1` round-trips overlap.
///
/// Assertion: the best widened-pool run completes faster than the
/// 1-worker run on the same total op count. We don't assert exact
/// speed-up ratios, and we don't require the widest condition to win —
/// flaky on shared CI runners where scheduling overhead can dominate
/// tiny debug-mode SELECT workloads. The point is that the pool is not
/// silently serializing.
#[tokio::test]
async fn bench_max_size_scaling() {
    banner("bench_max_size_scaling");
    let (cleanup, url) = provision_test_db().await;

    // Total ops are held constant across all conditions; per-worker
    // count = TOTAL_OPS / workers. With this shape the 1-worker and
    // 64-worker conditions push the *same* total work through the
    // pool, and only wall-clock differences are attributable to
    // concurrency.
    let total_ops = iter_scale(1280, 256);
    let widths = [1usize, 4, 16, 64];

    println!("total_ops = {total_ops} (held constant across all conditions)");
    println!("{:>8}  {:>12}  {:>14}", "workers", "wall (ms)", "ops/sec");

    let mut wall_per_width: Vec<(usize, Duration, u64)> = Vec::with_capacity(widths.len());

    for &workers in &widths {
        assert!(
            total_ops.is_multiple_of(workers),
            "total_ops ({total_ops}) must divide evenly by workers ({workers}) \
             to keep per-condition op counts comparable"
        );
        let ops_per_worker = total_ops / workers;

        let pool = Arc::new(
            DjogiPool::builder(&url)
                .max_size(workers)
                .build()
                .await
                .expect("pool builds"),
        );

        // Pre-warm: force `workers` physical connections to open
        // before the timer starts, so wall-clock measures concurrent
        // SELECTs and not serial socket creation. Without this the
        // 64-worker condition spends ~64 × connect-RTT in startup
        // before any query work overlaps, which would swamp the
        // genuine concurrency win.
        let warmup_futs = (0..workers).map(|_| {
            let pool = pool.clone();
            tokio::spawn(async move {
                pool.raw_with_client(|client| {
                    Box::pin(async move {
                        let _ = client
                            .simple_query("SELECT 1")
                            .await
                            .map_err(djogi::DjogiError::from)?;
                        // Hold the slot open until the rest of the
                        // pool's slots are also primed; otherwise
                        // each warmup task would just hit slot 0.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok::<_, DjogiError>(())
                    })
                })
                .await
                .expect("warmup checkout succeeds");
            })
        });
        for join in join_all(warmup_futs).await {
            join.expect("warmup joins clean");
        }

        let start = Instant::now();
        let futs = (0..workers).map(|_| {
            let pool = pool.clone();
            tokio::spawn(async move {
                for _ in 0..ops_per_worker {
                    pool.raw_with_client(|client| {
                        Box::pin(async move {
                            let _ = client
                                .simple_query("SELECT 1")
                                .await
                                .map_err(djogi::DjogiError::from)?;
                            Ok::<_, DjogiError>(())
                        })
                    })
                    .await
                    .expect("checkout succeeds");
                }
            })
        });
        for join in join_all(futs).await {
            join.expect("worker joins clean");
        }
        let elapsed = start.elapsed();

        let total = u64::try_from(total_ops).unwrap_or(u64::MAX);
        let secs = elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
        let throughput = (total as f64) / secs;

        println!(
            "{:>8}  {:>12.2}  {:>14.0}",
            workers,
            elapsed.as_secs_f64() * 1000.0,
            throughput
        );

        wall_per_width.push((workers, elapsed, total));
    }

    // Concurrency assertion — same total op count in every condition.
    // At least one widened-pool condition should beat the serial
    // wall-clock if the pool is genuinely overlapping IO across slots.
    // We keep measuring 64 workers, but do not require it specifically
    // to be the winner; in debug mode, task scheduling overhead can
    // swamp this tiny SELECT workload on loaded machines.
    let serial = wall_per_width
        .iter()
        .find(|(w, _, _)| *w == 1)
        .copied()
        .expect("serial run recorded");
    let best_parallel = wall_per_width
        .iter()
        .filter(|(w, _, _)| *w > 1)
        .min_by_key(|(_, elapsed, _)| *elapsed)
        .copied()
        .expect("parallel run recorded");

    println!(
        "serial 1-worker total = {:.2}ms; \
         best widened condition = {} workers at {:.2}ms \
         (same {} total ops in every condition)",
        serial.1.as_secs_f64() * 1000.0,
        best_parallel.0,
        best_parallel.1.as_secs_f64() * 1000.0,
        serial.2,
    );

    assert!(
        best_parallel.1 < serial.1,
        "best widened-pool wall-clock ({} workers, {:?}) should beat \
         1-worker wall-clock ({:?}) on identical total op count — pool is \
         not delivering concurrency",
        best_parallel.0,
        best_parallel.1,
        serial.1,
    );

    teardown_test_db(cleanup).await;
}

// ---------------------------------------------------------------------------
// 2. post_connect overhead
// ---------------------------------------------------------------------------

/// Measures the cost of running a one-round-trip `post_connect` callback
/// during physical-connection creation. We force fresh-connection
/// creation on each iteration by:
///
/// - keeping `max_size = 1`, so the pool never has more than one slot,
/// - calling `pool.close()` between iterations so the next checkout has
///   to open a new physical connection.
///
/// This isolates the connection-startup path. Both pools time the same
/// number of fresh acquisitions; pool B additionally runs a single
/// `SET application_name` round-trip in `post_connect`.
///
/// Bound: pool B mean stays within 3× of pool A mean. The 3× is a smoke
/// bound — we only catch the catastrophic-regression class, not subtle
/// perf drift.
#[tokio::test]
async fn bench_post_connect_overhead() {
    banner("bench_post_connect_overhead");
    let (cleanup, url) = provision_test_db().await;

    let iterations = iter_scale(50, 10);

    async fn time_fresh_acquisitions(url: &str, with_hook: bool, iterations: usize) -> Duration {
        let mut total = Duration::ZERO;
        for _ in 0..iterations {
            // Build a fresh pool each iteration — `max_size = 1` plus
            // `pool.close()` after the single checkout below guarantees
            // the connection is physical-fresh every time.
            let pool = if with_hook {
                DjogiPool::builder(url)
                    .max_size(1)
                    .post_connect(|client| {
                        Box::pin(async move {
                            client
                                .batch_execute("SET application_name = 'djogi_bench'")
                                .await
                                .map_err(djogi::DjogiError::from)?;
                            Ok(())
                        })
                    })
                    .build()
                    .await
                    .expect("pool builds")
            } else {
                DjogiPool::builder(url)
                    .max_size(1)
                    .build()
                    .await
                    .expect("pool builds")
            };

            let start = Instant::now();
            pool.raw_with_client(|client| {
                Box::pin(async move {
                    let _ = client
                        .simple_query("SELECT 1")
                        .await
                        .map_err(djogi::DjogiError::from)?;
                    Ok::<_, DjogiError>(())
                })
            })
            .await
            .expect("first checkout = physical create + post_connect + SELECT 1");
            total += start.elapsed();

            // Drop the pool so the next iteration's connection is fresh.
            drop(pool);
        }
        total / u32::try_from(iterations).expect("iterations fits u32")
    }

    let mean_no_hook = time_fresh_acquisitions(&url, false, iterations).await;
    let mean_with_hook = time_fresh_acquisitions(&url, true, iterations).await;

    println!("iterations per condition = {iterations}");
    println!("mean per fresh acquisition (no hook):   {mean_no_hook:?}");
    println!("mean per fresh acquisition (with hook): {mean_with_hook:?}");
    let ratio = mean_with_hook.as_secs_f64() / mean_no_hook.as_secs_f64().max(f64::MIN_POSITIVE);
    println!("ratio with_hook / no_hook = {ratio:.2}×");

    // Loose smoke bound — we want to catch a 100× regression, not a 1.2×
    // drift. Networking dominates the absolute number on most hosts, so
    // even a real one-round-trip callback usually lands within ~1.1-1.5×.
    assert!(
        ratio < 3.0,
        "post_connect callback ratio {ratio:.2}× exceeds 3× smoke bound: \
         no_hook={mean_no_hook:?}, with_hook={mean_with_hook:?}"
    );

    teardown_test_db(cleanup).await;
}

// ---------------------------------------------------------------------------
// 3. with_client per-op overhead vs persistent client
// ---------------------------------------------------------------------------

/// Compares `with_client` (acquire + release per op) against holding a
/// single client across N ops. The ratio quantifies the price the
/// dirty-by-default RAII guard charges per op — the safety guarantee
/// being that a panic / `Err` detaches a poisoned client so it never
/// re-enters the pool.
///
/// We do not assert a tight ratio: the absolute cost depends entirely
/// on connection-checkout machinery vs direct-call dispatch and is
/// host-sensitive. Instead we print the numbers so the ratio is
/// grep-able from CI logs and assert only that `with_client` doesn't
/// somehow take *less* time than the persistent path (which would
/// indicate the bench is measuring nothing).
#[tokio::test]
async fn bench_with_client_vs_persistent() {
    banner("bench_with_client_vs_persistent");
    let (cleanup, url) = provision_test_db().await;

    let ops = iter_scale(1000, 100);

    let pool = DjogiPool::builder(&url)
        .max_size(1)
        .build()
        .await
        .expect("pool builds");

    // Path A: one with_client per op. Each iteration acquires from the
    // pool, runs SELECT 1, returns the client.
    let start = Instant::now();
    for _ in 0..ops {
        pool.raw_with_client(|client| {
            Box::pin(async move {
                let _ = client
                    .simple_query("SELECT 1")
                    .await
                    .map_err(djogi::DjogiError::from)?;
                Ok::<_, DjogiError>(())
            })
        })
        .await
        .expect("with_client checkout");
    }
    let with_client_total = start.elapsed();

    // Path B: hold a single client across all N ops by entering
    // `with_client` exactly once and looping inside the closure. The
    // pool's checkout machinery only fires twice (acquire + release).
    let start = Instant::now();
    pool.raw_with_client(|client| {
        Box::pin(async move {
            for _ in 0..ops {
                let _ = client
                    .simple_query("SELECT 1")
                    .await
                    .map_err(djogi::DjogiError::from)?;
            }
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("persistent checkout");
    let persistent_total = start.elapsed();

    let with_client_per_op = with_client_total / u32::try_from(ops).expect("ops fits u32");
    let persistent_per_op = persistent_total / u32::try_from(ops).expect("ops fits u32");
    let overhead_ratio =
        with_client_total.as_secs_f64() / persistent_total.as_secs_f64().max(f64::MIN_POSITIVE);

    println!("ops = {ops}");
    println!("with_client total = {with_client_total:?} (per-op: {with_client_per_op:?})");
    println!("persistent total  = {persistent_total:?} (per-op: {persistent_per_op:?})");
    println!("overhead ratio with_client / persistent = {overhead_ratio:.2}×");
    println!("this ratio is the price for the dirty-by-default with_client RAII guard");

    // Sanity floor: with_client should not be faster than the
    // persistent path. If it were, the bench is measuring noise and
    // any future regression assertion would be meaningless.
    assert!(
        with_client_total >= persistent_total,
        "with_client ({with_client_total:?}) must not beat persistent \
         ({persistent_total:?}) — bench is not measuring acquire/release \
         overhead correctly"
    );

    teardown_test_db(cleanup).await;
}
