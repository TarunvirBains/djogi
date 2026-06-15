// Issue #168 — typed isolation-level surface
// (`djogi::transaction::atomic_with` + `IsolationLevel`).
//
// # Scope
//
// Cover the live-PG behaviours of `atomic_with`:
//
// 1. **Each isolation variant successfully opens a transaction.**
//  Postgres parses `BEGIN ISOLATION LEVEL <kw>` and rejects
//  malformed keywords with a syntax error; a successful BEGIN +
//  completed closure proves the framework's SQL composition
//  survives the round trip. The level-keyword pins in
//  `djogi/src/transaction.rs::tests::begin_with_isolation_sql_*`
//  cover the textual emission; this file proves Postgres accepts
//  the emitted text.
//
// 2. **Observed `transaction_isolation` matches the requested
//  [`IsolationLevel`]** inside the open transaction. This is the
//  a high-severity review xhigh follow-up: the prior coverage only proved BEGIN
//  didn't syntax-error, not that the requested level actually
//  bound to the session. Read the `current_setting('transaction_isolation')`
//  GUC back inside `atomic_with`. Uses
//  `#[djogi::deliberately_bypass_convention_with_raw_sql]` because
//  djogi has no typed `SHOW` / `current_setting` surface today
//  (tracking: djogi#168). JUSTIFICATION comments attach to the
//  decorated test items per CLAUDE.md raw-SQL convention.
//
// 3. **Pool-path commit and rollback semantics mirror `atomic`** —
//  writes persist on Ok, vanish on Err.
//
// 4. **Nested `atomic_with(level, &mut tx_ctx, ...)` rejects with
//  `DjogiError::IsolationLevelOnNestedScope`.** Postgres pins
//  isolation at the outer BEGIN; the typed-error rejection
//  surfaces synchronously before any SQL flies, mirroring the
//  `SetRoleOutsideTransaction` discipline.
//
// 5. **`retry_on_conflict` composes with `atomic_with` for the
//  `Serializable` / `RepeatableRead` SQLSTATE-40001 retry loop.**
//  Verified by wiring a one-attempt `retry_on_conflict` around a
//  serializable atomic_with closure that runs to completion.
//
// 6. **Real 40001 retry through `atomic_with` + `retry_on_conflict`.**
//  a high-severity review xhigh follow-up: drive two concurrent SERIALIZABLE
//  transactions through SSI-incompatible reads/writes so Postgres
//  raises `40001` (serialization_failure) at COMMIT on the second
//  one. The retry budget then re-runs the closure (which observes
//  the first transaction's committed state and proceeds cleanly).
//  Without `retry_on_conflict` the 40001 would surface to the
//  caller; with it, the retry loop classifies and re-runs.
//
// # Spec / memory anchors
//
// - djogi#168 issue body (closing-condition checklist).
// - `docs/guide/transactions.md` §"Isolation levels — `atomic_with`".
// - `feedback_djogi_local_postgres.md` — `#[djogi_test]` provisions a
//  fresh DB per test.

use djogi::prelude::*;
use djogi::transaction::{IsolationLevel, atomic, atomic_with, retry_on_conflict};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Barrier;

#[model(table = "djogi_iso_widgets", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct IsoWidget {
    pub label: String,
}

/// Round-trip a single isolation level: open `atomic_with`, run a
/// small typed read inside the transaction to confirm the connection
/// is active, assert no error surfaces. The "the BEGIN composed
/// correctly" half — observation of the actual GUC value lives in
/// [`assert_atomic_with_observes_level`].
async fn assert_atomic_with_opens_at_level(ctx: &mut DjogiContext, level: IsolationLevel) {
    let observed = atomic_with(level, ctx, |tx| {
        Box::pin(async move {
            let _ = IsoWidget::objects().count(tx).await?;
            Ok::<_, DjogiError>(())
        })
    })
    .await;
    assert!(
        observed.is_ok(),
        "atomic_with({level}) must succeed; saw: {observed:?}",
    );
}

/// Map a [`IsolationLevel`] to the lowercase string Postgres returns
/// from `current_setting('transaction_isolation')`. The GUC returns
/// the level in lowercase regardless of how it was set:
///  `READ COMMITTED` → `"read committed"`
///  `REPEATABLE READ` → `"repeatable read"`
///  `SERIALIZABLE`  → `"serializable"`
fn expected_guc_for_level(level: IsolationLevel) -> &'static str {
    match level {
        IsolationLevel::ReadCommitted => "read committed",
        IsolationLevel::RepeatableRead => "repeatable read",
        IsolationLevel::Serializable => "serializable",
    }
}

/// Open `atomic_with(level, ...)` and read back
/// `current_setting('transaction_isolation')` from inside the
/// transaction — proves the requested level actually bound to the
/// session, not just that BEGIN didn't syntax-error.
///
/// This is the a high-severity review xhigh fix for djogi#168 finding #4: prior
/// coverage proved the SQL parsed; this proves the level applies at
/// runtime. The raw-SQL bypass is justified inline at every call
/// site because djogi does not (yet) expose a typed `SHOW` /
/// `current_setting` surface.
#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#168): djogi has no typed `SHOW` /
// `current_setting` surface today; reading `transaction_isolation`
// to prove the requested level actually bound to the session is
// runtime introspection of a Postgres GUC. Adding a typed
// `current_setting<T>` accessor is tracked separately. Without
// observation the prior coverage only proved BEGIN didn't error.
async fn assert_atomic_with_observes_level(ctx: &mut DjogiContext, level: IsolationLevel) {
    let expected = expected_guc_for_level(level);
    let observed: String = atomic_with(level, ctx, |tx| {
        Box::pin(async move {
            tx.raw_scalar::<String>("SELECT current_setting('transaction_isolation')", &[])
                .await
        })
    })
    .await
    .expect("atomic_with(level) must run the GUC read to completion");
    assert_eq!(
        observed, expected,
        "atomic_with({level}) must bind transaction_isolation = {expected:?}; \
     observed: {observed:?}"
    );
}

#[djogi::djogi_test(sync_models = [IsoWidget])]
async fn atomic_with_serializable_opens_transaction(mut ctx: djogi::DjogiContext) {
    // Pin the keyword acceptance end-to-end. If `BEGIN ISOLATION LEVEL
    // SERIALIZABLE` were malformed, Postgres would raise a syntax
    // error during the BEGIN — `atomic_with` would surface that as
    // an `Err`. The successful `Ok(())` proves the keyword survives
    // the Postgres parser.
    assert_atomic_with_opens_at_level(&mut ctx, IsolationLevel::Serializable).await;
}

#[djogi::djogi_test(sync_models = [IsoWidget])]
async fn atomic_with_repeatable_read_opens_transaction(mut ctx: djogi::DjogiContext) {
    assert_atomic_with_opens_at_level(&mut ctx, IsolationLevel::RepeatableRead).await;
}

#[djogi::djogi_test(sync_models = [IsoWidget])]
async fn atomic_with_read_committed_opens_transaction(mut ctx: djogi::DjogiContext) {
    assert_atomic_with_opens_at_level(&mut ctx, IsolationLevel::ReadCommitted).await;
}

// ---------------------------------------------------------------------------
// a high-severity review xhigh follow-up: observe the actual `transaction_isolation`
// GUC inside the open transaction. The prior tests only proved BEGIN
// did not syntax-error; these tests prove the requested level
// actually bound to the session.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [IsoWidget])]
async fn atomic_with_serializable_binds_session_isolation(mut ctx: djogi::DjogiContext) {
    assert_atomic_with_observes_level(&mut ctx, IsolationLevel::Serializable).await;
}

#[djogi::djogi_test(sync_models = [IsoWidget])]
async fn atomic_with_repeatable_read_binds_session_isolation(mut ctx: djogi::DjogiContext) {
    assert_atomic_with_observes_level(&mut ctx, IsolationLevel::RepeatableRead).await;
}

#[djogi::djogi_test(sync_models = [IsoWidget])]
async fn atomic_with_read_committed_binds_session_isolation(mut ctx: djogi::DjogiContext) {
    assert_atomic_with_observes_level(&mut ctx, IsolationLevel::ReadCommitted).await;
}

#[djogi::djogi_test(sync_models = [IsoWidget])]
async fn atomic_with_pool_path_commits_and_persists_writes(mut ctx: djogi::DjogiContext) {
    // The pool-backed-context atomic_with path must commit on Ok and
    // persist writes — same contract as `atomic` itself, just with
    // an explicit isolation level. Insert one row inside the
    // serializable scope; observe it post-commit on the outer
    // pool-backed context.
    atomic_with(IsolationLevel::Serializable, &mut ctx, |tx| {
        Box::pin(async move {
            IsoWidget::create(
                tx,
                IsoWidget {
                    label: "iso-row".to_string(),
                    ..Default::default()
                },
            )
            .await?;
            Ok::<_, DjogiError>(())
        })
    })
    .await
    .expect("atomic_with(Serializable) commit on Ok");

    let count = IsoWidget::objects()
        .count(&mut ctx)
        .await
        .expect("post-commit count");
    assert_eq!(count, 1, "atomic_with must commit writes on Ok");
}

#[djogi::djogi_test(sync_models = [IsoWidget])]
async fn atomic_with_pool_path_rolls_back_on_err(mut ctx: djogi::DjogiContext) {
    // Rollback semantics mirror `atomic` — Err inside the closure
    // discards the inserted row.
    let result: Result<(), DjogiError> =
        atomic_with(IsolationLevel::Serializable, &mut ctx, |tx| {
            Box::pin(async move {
                IsoWidget::create(
                    tx,
                    IsoWidget {
                        label: "iso-rollback".to_string(),
                        ..Default::default()
                    },
                )
                .await?;
                Err::<(), _>(DjogiError::not_found("forced rollback"))
            })
        })
        .await;
    assert!(result.is_err(), "closure Err must surface");

    let count = IsoWidget::objects()
        .count(&mut ctx)
        .await
        .expect("post-rollback count");
    assert_eq!(count, 0, "atomic_with must roll back writes on Err");
}

#[djogi::djogi_test(sync_models = [IsoWidget])]
async fn atomic_with_rejects_nested_savepoint_scope(mut ctx: djogi::DjogiContext) {
    // Open an outer `atomic` (default isolation), then attempt
    // `atomic_with` inside — the nested call must reject with
    // `IsolationLevelOnNestedScope` because Postgres pins isolation
    // at the outer BEGIN.
    let result = atomic(&mut ctx, |tx| {
        Box::pin(async move {
            let nested = atomic_with(IsolationLevel::Serializable, tx, |_inner| {
                Box::pin(async move { Ok::<_, DjogiError>(()) })
            })
            .await;

            match nested {
                Err(DjogiError::IsolationLevelOnNestedScope { requested }) => {
                    assert_eq!(
                        requested,
                        IsolationLevel::Serializable,
                        "rejection must carry the requested level for log scrapers",
                    );
                    Ok::<_, DjogiError>(())
                }
                other => panic!("expected IsolationLevelOnNestedScope, got {other:?}"),
            }
        })
    })
    .await;
    assert!(
        result.is_ok(),
        "outer atomic must complete after typed rejection"
    );
}

#[djogi::djogi_test(sync_models = [IsoWidget])]
async fn atomic_with_composes_with_retry_on_conflict(mut ctx: djogi::DjogiContext) {
    // `retry_on_conflict` + `atomic_with` is the canonical pattern
    // for serializable scopes that may raise 40001 at commit. Verify
    // the wrapper composes — the closure runs to completion, no
    // synthetic 40001 is forced here (the deterministic real-40001
    // path lives in
    // `atomic_with_with_retry_on_conflict_recovers_from_real_40001`
    // below; this test pins the no-conflict happy-path).
    let result = retry_on_conflict(&mut ctx, 2, async |ctx| {
        atomic_with(IsolationLevel::Serializable, ctx, |tx| {
            Box::pin(async move {
                IsoWidget::create(
                    tx,
                    IsoWidget {
                        label: "retry-iso".to_string(),
                        ..Default::default()
                    },
                )
                .await?;
                Ok::<_, DjogiError>(())
            })
        })
        .await
    })
    .await;
    assert!(
        result.is_ok(),
        "retry_on_conflict + atomic_with must compose: {result:?}",
    );

    let count = IsoWidget::objects()
        .count(&mut ctx)
        .await
        .expect("post-retry count");
    assert_eq!(
        count, 1,
        "retry_on_conflict + atomic_with must commit on Ok"
    );
}

// ---------------------------------------------------------------------------
// a high-severity review xhigh follow-up: real `40001` serialization-failure retry
// driven through `atomic_with(Serializable, ...)` + `retry_on_conflict`.
//
// Drives two concurrent SERIALIZABLE transactions through an SSI-
// incompatible read/write pattern:
//
//  1. Each transaction reads the current `IsoWidget` count.
//  2. A `tokio::sync::Barrier` of capacity 2 forces both readers to
//   observe their snapshots BEFORE either writes — Postgres needs
//   the two snapshots to be concurrent for SSI to flag the
//   anomaly at commit.
//  3. Each transaction inserts one row whose label depends on the
//   snapshot count it observed.
//  4. Both transactions commit. Under SSI, one commit succeeds and
//   the other raises `40001` because each transaction's write
//   makes the other's snapshot count stale.
//
// `retry_on_conflict(attempts = 4)` wraps each side. The loser's
// closure re-runs against the winner's committed state, observes
// `count = 1`, writes `"parity-1"`, and commits cleanly. Both sides
// return `Ok`; the post-`join!` row count is 2.
//
// `closure_runs` atomics count how many times each closure body
// executed. If no real 40001 occurred (e.g. the framework silently
// dropped the level, ran both at READ COMMITTED, and never raised),
// both counters stay at 1 and the test fails the
// "expected at least one retry" assertion.
//
// This is the deterministic SSI test: the barrier coordinates the
// transaction interleave, eliminating the wall-clock race that
// would make a naive `tokio::join!` flaky. The retry budget is sized
// for the worst case (4 attempts) so any plausible re-conflict in a
// constrained CI environment still resolves before the closure
// errors out.
// ---------------------------------------------------------------------------

#[djogi::djogi_test(sync_models = [IsoWidget])]
async fn atomic_with_with_retry_on_conflict_recovers_from_real_40001(mut ctx: djogi::DjogiContext) {
    let barrier = Arc::new(Barrier::new(2));
    let runs_a = Arc::new(AtomicUsize::new(0));
    let runs_b = Arc::new(AtomicUsize::new(0));

    // Two independent pool-backed contexts so the two `atomic_with`
    // calls run on DIFFERENT physical connections — the prerequisite
    // for two genuinely-concurrent transactions. `clone_for_concurrent_reads`
    // is the typed surface (#173) for exactly this shape.
    let mut ctx_a = ctx
        .clone_for_concurrent_reads()
        .expect("clone for concurrent-reads on a pool-backed context");
    let mut ctx_b = ctx
        .clone_for_concurrent_reads()
        .expect("clone for concurrent-reads on a pool-backed context");

    /// Per-transaction body. Reads the current count, awaits the
    /// shared barrier (both sides reach this point with their
    /// snapshots in flight), then inserts a row whose label encodes
    /// the observed count. SSI flags the anomaly at commit and one
    /// side raises 40001; `retry_on_conflict` re-runs the closure
    /// on the loser.
    async fn run_one(
        ctx: &mut DjogiContext,
        barrier: Arc<Barrier>,
        runs: Arc<AtomicUsize>,
        side: &'static str,
    ) -> Result<(), DjogiError> {
        retry_on_conflict(ctx, 4, async |ctx| {
            // The barrier must be re-armed every retry attempt OR
            // released only on the first attempt — using a single
            // 2-party barrier would deadlock on retry (no second
            // party arriving). The simplest correct shape: clone the
            // Arc but use `try_wait`-style: only the first call
            // waits; subsequent retries fall through.
            //
            // We use a counter to allow exactly one barrier rendezvous
            // per side — the first attempt synchronises with the
            // sibling, every retry runs solo against the now-committed
            // state from the winner.
            let attempt = runs.fetch_add(1, Ordering::SeqCst) + 1;
            atomic_with(IsolationLevel::Serializable, ctx, |tx| {
                let barrier = barrier.clone();
                Box::pin(async move {
                    // 1. Read snapshot.
                    let count_before = IsoWidget::objects().count(tx).await?;
                    tracing::debug!(
                        side,
                        attempt,
                        count_before,
                        "ssi-serializable: snapshot read",
                    );
                    // 2. First attempt synchronises with the other
                    //  side to guarantee concurrent snapshots.
                    //  Retries don't need the sync (the other side
                    //  has already committed by then).
                    if attempt == 1 {
                        barrier.wait().await;
                    }
                    // 3. Write a row whose label depends on the
                    //  observed snapshot. SSI flags the anomaly:
                    //  both sides read N, both write a row that
                    //  counts in the other's snapshot.
                    IsoWidget::create(
                        tx,
                        IsoWidget {
                            label: format!("parity-{side}-{count_before}"),
                            ..Default::default()
                        },
                    )
                    .await?;
                    Ok::<_, DjogiError>(())
                })
            })
            .await
        })
        .await
    }

    let task_a = run_one(&mut ctx_a, barrier.clone(), runs_a.clone(), "A");
    let task_b = run_one(&mut ctx_b, barrier.clone(), runs_b.clone(), "B");

    let (a, b) = tokio::join!(task_a, task_b);

    a.expect("side A must commit after retry_on_conflict recovers from any 40001");
    b.expect("side B must commit after retry_on_conflict recovers from any 40001");

    // Both rows must persist. Two distinct labels, two distinct rows.
    let count = IsoWidget::objects()
        .count(&mut ctx)
        .await
        .expect("post-join count");
    assert_eq!(
        count,
        2,
        "both transactions must commit a row after SSI retry resolves; \
     saw {count} rows (runs_a = {a_runs}, runs_b = {b_runs})",
        a_runs = runs_a.load(Ordering::SeqCst),
        b_runs = runs_b.load(Ordering::SeqCst),
    );

    // The deterministic proof of the real 40001 path: at least one
    // side ran its closure more than once. Postgres' SSI is
    // intentionally probabilistic about which side wins, so we
    // assert the symmetric "at least one retry happened" rather than
    // hard-coding A or B. If both counters are 1, no 40001 ever
    // surfaced — the test failed to drive the path we are pinning.
    let a_runs = runs_a.load(Ordering::SeqCst);
    let b_runs = runs_b.load(Ordering::SeqCst);
    assert!(
        a_runs >= 2 || b_runs >= 2,
        "expected at least one side to re-run its closure after a real 40001 \
     (runs_a = {a_runs}, runs_b = {b_runs}). If both counters are 1, the \
     test environment did not surface a serialization failure — the \
     barrier-coordinated SSI scenario must drive Postgres to raise it.",
    );
}
