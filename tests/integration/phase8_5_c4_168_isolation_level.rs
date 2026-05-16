// Phase 8.5 Cluster 4 issue #168 — typed isolation-level surface
// (`djogi::transaction::atomic_with` + `IsolationLevel`).
//
// # Scope
//
// Cover the four live-PG behaviours of `atomic_with`:
//
// 1. **Each isolation variant successfully opens a transaction.**
//    Postgres parses `BEGIN ISOLATION LEVEL <kw>` and rejects
//    malformed keywords with a syntax error; a successful BEGIN +
//    completed closure proves the framework's SQL composition
//    survives the round trip. The level-keyword pins in
//    `djogi/src/transaction.rs::tests::begin_with_isolation_sql_*`
//    cover the textual emission; this file proves Postgres accepts
//    the emitted text.
//
// 2. **Pool-path commit and rollback semantics mirror `atomic`** —
//    writes persist on Ok, vanish on Err.
//
// 3. **Nested `atomic_with(level, &mut tx_ctx, ...)` rejects with
//    `DjogiError::IsolationLevelOnNestedScope`.** Postgres pins
//    isolation at the outer BEGIN; the typed-error rejection
//    surfaces synchronously before any SQL flies, mirroring the
//    `SetRoleOutsideTransaction` discipline.
//
// 4. **`retry_on_conflict` composes with `atomic_with` for the
//    `Serializable` / `RepeatableRead` SQLSTATE-40001 retry loop.**
//    Verified by wiring a one-attempt `retry_on_conflict` around a
//    serializable atomic_with closure that runs to completion —
//    the surface composes; the actual 40001-retry path is exercised
//    in `phase4_transactions_expressions.rs` via `LockConflict`.
//
// # Spec / memory anchors
//
// - djogi#168 issue body (closing-condition checklist).
// - `docs/guide/transactions.md` §"Isolation levels — `atomic_with`".
// - `feedback_djogi_local_postgres.md` — `#[djogi_test]` provisions a
//   fresh DB per test.

use djogi::prelude::*;
use djogi::transaction::{IsolationLevel, atomic, atomic_with, retry_on_conflict};

#[model(table = "djogi_iso_widgets", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct IsoWidget {
    pub label: String,
}

/// Round-trip a single isolation level: open `atomic_with`, run a
/// small typed read inside the transaction to confirm the connection
/// is active, assert no error surfaces.
///
/// Reading the Postgres `transaction_isolation` GUC back is a
/// session-introspection that the typed surface does not expose
/// today; this file pairs with the unit tests at
/// `djogi/src/transaction.rs::tests::begin_with_isolation_sql_*`
/// (textual composition) to anchor the SQL end-to-end. Postgres
/// rejects malformed BEGIN grammar at parse time, so a successful
/// `atomic_with` open + typed read proves the keyword survives the
/// parser.
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
    // synthetic 40001 is forced here (that surface is exercised by
    // `phase4_transactions_expressions.rs` via LockConflict).
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
