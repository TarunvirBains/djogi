//! Publisher → subscriber `LISTEN/NOTIFY` round-trip.
//!
//! The `rollback_suppresses_notify` test isolates a behaviour worth
//! pinning explicitly: `pg_notify` payloads are buffered by Postgres
//! until `COMMIT` and discarded on `ROLLBACK`, so subscribers must
//! never see an event for a rolled-back transaction.

use djogi::notify::{EventKind, subscribe};
use djogi::prelude::*;
use std::time::Duration;
use tokio::time::timeout;

#[model(table = "evt", pk = HeerId, events)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct EvtRow {
    pub label: String,
}

/// Wall-clock bound on every `recv().await` so a publisher mishap
/// can't wedge the test forever. 5 s is generous for a localhost
/// round-trip; CI on slow runners has been seen to take ~1 s for
/// the LISTEN → NOTIFY round-trip on first warm-up, so 5 s is the
/// "definitely broken if exceeded" threshold rather than a tight
/// budget.
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

// ── Test 1 — full publisher → subscriber round-trip ─────────────────────────
#[djogi::djogi_test(sync_models = [EvtRow])]
async fn create_save_delete_roundtrip(mut ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");

    // Subscribe BEFORE the first write so the LISTEN registration
    // races ahead of every NOTIFY we'll emit. The async LISTEN
    // returns only after Postgres acknowledges the registration, so
    // we know subsequent NOTIFY events from this pool will be
    // routed.
    let mut rx = subscribe::<EvtRow>(&pool)
        .await
        .expect("subscribe must succeed against a reachable pool with a URL");

    // ── 1. CREATE ─────────────────────────────────────────────────────────
    let row = djogi::transaction::atomic(&pool, |inner| {
        Box::pin(async move {
            EvtRow::create(
                inner,
                EvtRow {
                    id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                    created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                    updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                    label: "alpha".to_string(),
                },
            )
            .await
        })
    })
    .await
    .expect("create EvtRow");
    let row_id = row.id;

    let event = timeout(RECV_TIMEOUT, rx.recv())
        .await
        .expect("recv must arrive within RECV_TIMEOUT after a committed CREATE")
        .expect("recv must decode the create event");
    assert_eq!(event.kind, EventKind::Created);
    assert_eq!(
        event.id, row_id,
        "round-trip id must match the CREATE'd row id"
    );

    // ── 2. SAVE (UPDATE) ──────────────────────────────────────────────────
    let mut row = row;
    row.label = "alpha-edited".to_string();
    djogi::transaction::atomic(&pool, |inner| {
        Box::pin(async move { row.save(inner).await })
    })
    .await
    .expect("save EvtRow");

    let event = timeout(RECV_TIMEOUT, rx.recv())
        .await
        .expect("recv must arrive within RECV_TIMEOUT after a committed SAVE")
        .expect("recv must decode the save event");
    assert_eq!(
        event.kind,
        EventKind::Updated,
        "wire 'save' must surface as EventKind::Updated"
    );
    assert_eq!(event.id, row_id);

    // ── 3. DELETE ─────────────────────────────────────────────────────────
    // Re-fetch since we consumed `row` in the SAVE step above.
    let row = EvtRow::get(&mut ctx, row_id)
        .await
        .expect("re-fetch EvtRow before delete");
    djogi::transaction::atomic(&pool, |inner| {
        Box::pin(async move { row.delete(inner).await })
    })
    .await
    .expect("delete EvtRow");

    let event = timeout(RECV_TIMEOUT, rx.recv())
        .await
        .expect("recv must arrive within RECV_TIMEOUT after a committed DELETE")
        .expect("recv must decode the delete event");
    assert_eq!(event.kind, EventKind::Deleted);
    assert_eq!(event.id, row_id);
}

// ── Test 2 — rolled-back transaction emits no notification ──────────────────
//
// `pg_notify` payloads inside a transaction are buffered until COMMIT
// and discarded on ROLLBACK. Pin that contract: a model write that
// rolls back must NOT surface a subscriber event.
#[djogi::djogi_test(sync_models = [EvtRow])]
async fn rollback_suppresses_notify(mut ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");

    let mut rx = subscribe::<EvtRow>(&pool)
        .await
        .expect("subscribe must succeed");

    let result: Result<(), djogi::DjogiError> = djogi::transaction::atomic(&pool, |inner| {
        Box::pin(async move {
            let _ = EvtRow::create(
                inner,
                EvtRow {
                    id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                    created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                    updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                    label: "rollback-me".to_string(),
                },
            )
            .await?;
            Err(djogi::DjogiError::Validation(
                "deliberate rollback for round-trip test".to_string(),
            ))
        })
    })
    .await;
    assert!(
        result.is_err(),
        "atomic body returned Err — transaction must have rolled back"
    );

    // 500 ms — comfortably past the localhost NOTIFY round-trip if
    // rollback failed to suppress it.
    let outcome = timeout(Duration::from_millis(500), rx.recv()).await;
    assert!(
        outcome.is_err(),
        "rollback'd transaction must not surface a notify event; \
     got an event when none was expected: {:?}",
        outcome.ok().and_then(|r| r.ok().map(|e| (e.kind, e.id)))
    );
}

#[djogi::djogi_test(sync_models = [EvtRow])]
async fn multiple_subscribers_fan_out(mut ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");

    let mut rx_a = subscribe::<EvtRow>(&pool).await.expect("subscribe A");
    let mut rx_b = subscribe::<EvtRow>(&pool).await.expect("subscribe B");

    let row = djogi::transaction::atomic(&pool, |inner| {
        Box::pin(async move {
            EvtRow::create(
                inner,
                EvtRow {
                    id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                    created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                    updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                    label: "fan-out".to_string(),
                },
            )
            .await
        })
    })
    .await
    .expect("create EvtRow");

    let event_a = timeout(RECV_TIMEOUT, rx_a.recv())
        .await
        .expect("subscriber A must receive event")
        .expect("subscriber A decode");
    let event_b = timeout(RECV_TIMEOUT, rx_b.recv())
        .await
        .expect("subscriber B must receive event")
        .expect("subscriber B decode");

    assert_eq!(event_a.kind, EventKind::Created);
    assert_eq!(event_b.kind, EventKind::Created);
    assert_eq!(event_a.id, row.id);
    assert_eq!(event_b.id, row.id);
}

/// When the last `TypedReceiver` drops, the listener winds down (registry's
/// `Weak<PgListener>` becomes dangling). A subsequent `subscribe` reaps
/// the dangling slot and spawns a fresh listener — adopter-invisible.
#[djogi::djogi_test(sync_models = [EvtRow])]
async fn listener_hot_reload_after_last_subscriber_drops(mut ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");

    {
        let mut rx = subscribe::<EvtRow>(&pool).await.expect("subscribe round 1");

        let row = djogi::transaction::atomic(&pool, |inner| {
            Box::pin(async move {
                EvtRow::create(
                    inner,
                    EvtRow {
                        id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                        created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                        updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                        label: "round".to_string(),
                    },
                )
                .await
            })
        })
        .await
        .expect("create round 1");

        let event = timeout(RECV_TIMEOUT, rx.recv())
            .await
            .expect("round recv must arrive")
            .expect("round decode");
        assert_eq!(event.kind, EventKind::Created);
        assert_eq!(event.id, row.id);
        // `rx` drops at end of this block → listener strong count → 0.
    }

    let mut rx = subscribe::<EvtRow>(&pool)
        .await
        .expect("subscribe round 2 must succeed (hot-reload)");

    let row = djogi::transaction::atomic(&pool, |inner| {
        Box::pin(async move {
            EvtRow::create(
                inner,
                EvtRow {
                    id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                    created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                    updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                    label: "round".to_string(),
                },
            )
            .await
        })
    })
    .await
    .expect("create round 2");

    let event = timeout(RECV_TIMEOUT, rx.recv())
        .await
        .expect("round recv must arrive on the freshly-spawned listener")
        .expect("round decode");
    assert_eq!(event.kind, EventKind::Created);
    assert_eq!(event.id, row.id);
}
