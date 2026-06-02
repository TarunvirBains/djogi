// T2.7 — `on_commit` on a pool-backed `DjogiContext` is an
// audit-warn no-op rather than a silent drop.
//
// Closes T2 cluster counter-signal #3 surfaced by Codex T1 round-1
// WARN-1: queued `on_commit` callbacks were silently dropped on
// pool-backed contexts because no `commit()` ever runs to drain them.
// The §D3 canonical sequence (`before → DB → outbox → after →
// on_commit drain`) only fires inside `atomic()` (or an explicit
// `commit()` on a transaction-backed context). T2.7 mirrors the
// `_insecurely` audit-warn pattern: pool-backed callers emit
// a `#[track_caller] tracing::warn!` with the grep-able token
// `djogi::on_commit::pool_backed_drop` and the callback is dropped;
// transaction-backed callers see unchanged FIFO queueing + drain.
//
// # Tracing log assertions
//
// The log-capture pattern follows `auth.rs`: install the
// `tracing_test` global subscriber inline (via `tracing_test::internal`)
// rather than via the `#[traced_test]` attribute macro, because
// `#[djogi_test]` rewrites the test body into an inner function and
// `logs_contain` would land in the outer scope. Snapshot the global
// buffer length before the action under test so assertions are scoped
// to lines emitted by that action.

use djogi::DjogiError;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// ── tracing capture helpers (mirrors auth.rs) ──────────────────────

static LOG_CAPTURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Install the `tracing_test` global subscriber once and return the current
/// byte length of the global buffer. Subsequent `logs_since_contain` calls
/// scope their search to lines appended after this snapshot.
fn init_log_capture() -> usize {
    tracing_test::internal::INITIALIZED.call_once(|| {
        let buf = tracing_test::internal::global_buf();
        let mock_writer = tracing_test::internal::MockWriter::new(buf);
        let subscriber = tracing_test::internal::get_subscriber(mock_writer, "trace");
        tracing::dispatcher::set_global_default(subscriber).unwrap_or(());
    });
    tracing_test::internal::global_buf().lock().unwrap().len()
}

/// Return `true` if any line appended since byte offset `since` contains
/// `needle`.
fn logs_since_contain(since: usize, needle: &str) -> bool {
    let buf = tracing_test::internal::global_buf().lock().unwrap();
    let text = std::str::from_utf8(&buf[since..]).unwrap_or("");
    text.lines().any(|line| line.contains(needle))
}

/// Count the number of lines appended since byte offset `since` that
/// contain `needle`. Used to assert the warn is single-shot per
/// `on_commit` call (not per row, not per drain step).
fn logs_since_count(since: usize, needle: &str) -> usize {
    let buf = tracing_test::internal::global_buf().lock().unwrap();
    let text = std::str::from_utf8(&buf[since..]).unwrap_or("");
    text.lines().filter(|line| line.contains(needle)).count()
}

// ── pool-backed audit-warn ──────────────────────────────────────────────────

/// `on_commit` on a pool-backed context emits a single `tracing::warn!`
/// with the grep-able token `djogi::on_commit::pool_backed_drop` and
/// drops the callback (does not invoke it).
#[djogi::djogi_test]
async fn on_commit_pool_backed_emits_warn_and_drops_callback(mut ctx: djogi::DjogiContext) {
    // Sanity: the canonical `#[djogi_test]` fixture is pool-backed by default.
    assert!(
        ctx.raw_pool().is_some(),
        "test fixture must be pool-backed for this assertion to be meaningful"
    );

    let _log_guard = LOG_CAPTURE_LOCK.lock().await;
    let since = init_log_capture();
    let fired = Arc::new(AtomicBool::new(false));

    {
        let fired = fired.clone();
        ctx.on_commit(move || {
            let fired = fired.clone();
            async move {
                fired.store(true, Ordering::SeqCst);
                Ok::<(), DjogiError>(())
            }
        });
    }

    assert!(
        logs_since_contain(since, "djogi::on_commit::pool_backed_drop"),
        "expected warn log with grep-able token djogi::on_commit::pool_backed_drop"
    );
    assert_eq!(
        logs_since_count(since, "djogi::on_commit::pool_backed_drop"),
        1,
        "warn must be single-shot per on_commit call (not per row, not per drain)"
    );
    assert!(
        !fired.load(Ordering::SeqCst),
        "callback must NOT have run on a pool-backed context — there is no commit to drain it"
    );
}

/// Multiple `on_commit` calls on the same pool-backed context emit one
/// warn each (single-shot per call, not coalesced across calls).
#[djogi::djogi_test]
async fn on_commit_pool_backed_warn_per_call(mut ctx: djogi::DjogiContext) {
    assert!(ctx.raw_pool().is_some());
    let _log_guard = LOG_CAPTURE_LOCK.lock().await;
    let since = init_log_capture();

    ctx.on_commit(|| async { Ok::<(), DjogiError>(()) });
    ctx.on_commit(|| async { Ok::<(), DjogiError>(()) });
    ctx.on_commit(|| async { Ok::<(), DjogiError>(()) });

    assert_eq!(
        logs_since_count(since, "djogi::on_commit::pool_backed_drop"),
        3,
        "three on_commit calls on a pool-backed context must emit three warns"
    );
}

// ── transaction-backed regression ───────────────────────────────────────────

/// `on_commit` inside `atomic()` (transaction-backed) MUST NOT emit the
/// pool-backed warn, AND the callback MUST run after the outer commit.
/// This pins the existing behavior so the T2.7 audit-warn does
/// not regress the transactional path.
#[djogi::djogi_test]
async fn on_commit_inside_atomic_no_warn_and_callback_runs(mut ctx: djogi::DjogiContext) {
    let pool = ctx.raw_pool().expect("test ctx must be pool-backed").clone();
    let _log_guard = LOG_CAPTURE_LOCK.lock().await;
    let since = init_log_capture();
    let fired = Arc::new(AtomicUsize::new(0));

    {
        let fired = fired.clone();
        djogi::transaction::atomic(&pool, |tx| {
            Box::pin(async move {
                let fired = fired.clone();
                tx.on_commit(move || {
                    let fired = fired.clone();
                    async move {
                        fired.fetch_add(1, Ordering::SeqCst);
                        Ok::<(), DjogiError>(())
                    }
                });
                Ok::<_, DjogiError>(())
            })
        })
        .await
        .unwrap();
    }

    assert_eq!(
        logs_since_count(since, "djogi::on_commit::pool_backed_drop"),
        0,
        "transaction-backed on_commit must NOT emit the pool-backed warn"
    );
    assert_eq!(
        fired.load(Ordering::SeqCst),
        1,
        "transaction-backed on_commit callback must fire exactly once after outer commit"
    );
}
