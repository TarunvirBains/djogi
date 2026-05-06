//! Phase 8δ T8.7 integration tests: outbox-derived tombstones (Pattern 2)
//! in the delta-sync fetcher.
//!
//! # What this file pins
//!
//! 1. **`hard_delete_propagates_via_outbox_to_tombstone`** — creates a
//!    model with `#[model(events)]` but NOT `soft_deletable`. Inserts a
//!    row, runs a first tick (which initialises the per-fetcher outbox
//!    watermark to "now"). Hard-deletes the row in a transaction —
//!    `emit_event` writes `action='delete'` into the
//!    `<table>_outbox` table inside the same transaction. A second
//!    tick observes the outbox row, decodes
//!    `row_id BIGINT → HeerId → T::Id`, and routes it into
//!    `DeltaResult.tombstones`. Punnu's `apply_delta` evicts the entry;
//!    `punnu.get(id) == None`.
//!
//! 2. **`non_events_model_no_outbox_poll`** — backward-compat. A model
//!    without `events` does not emit outbox rows; the fetcher's
//!    outbox-poll path is gated off via
//!    `T::descriptor().has_outbox == false`. A regression that ran the
//!    poll anyway would error on the missing
//!    `phase8_t8_7_plain_row_outbox` table — this test passing is the
//!    structural proof the gate works.
//!
//! # Spec anchor
//!
//! `docs/superpowers/plans/granular-phase8/cluster-8delta-granular.md`
//! §3 T8.7. Original plan sketched a `broadcast::Receiver<...>`
//! subscription; investigation in GH #128 revealed djogi's outbox is
//! table-based (`{table}_outbox`), not channel-based. T8.7 implementation
//! adopts Option B (per-tick poll of the outbox table). The worker uses
//! state transitions (`pending → processing → published / failed`), never
//! `DELETE`, so the fetcher's poll never races the worker.
//!
//! # Fixture strategy
//!
//! Each test provisions its own table inline via `ctx.raw_execute`. The
//! `#[djogi_test]` macro installs HeeRanjID schema, seeds node 1, and sets
//! `heer.node_id = '1'` before the test body runs.
//!
//! Outbox table follows the Phase 4 schema:
//! `(id BIGINT PK DEFAULT generate_id(), row_id BIGINT, action TEXT,
//! payload JSONB, created_at TIMESTAMPTZ DEFAULT now())` — matches what
//! `djogi/src/outbox/mod.rs::insert_sql` writes into.

use djogi::prelude::*;

// ── Fixture model 1 — events-enabled, hard-delete tombstone source ──────────
#[model(table = "phase8_t8_7_evt_row", pk = HeerId, events)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct EventRow {
    pub label: String,
}

async fn setup_event_row(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE IF NOT EXISTS phase8_t8_7_evt_row (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            label       TEXT        NOT NULL
        )",
        &[],
    )
    .await
    .expect("create phase8_t8_7_evt_row table");

    ctx.raw_execute(
        "CREATE TABLE IF NOT EXISTS phase8_t8_7_evt_row_outbox (
            id         BIGINT      PRIMARY KEY DEFAULT generate_id(),
            row_id     BIGINT      NOT NULL,
            action     TEXT        NOT NULL,
            payload    JSONB       NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
        &[],
    )
    .await
    .expect("create phase8_t8_7_evt_row_outbox table");

    ctx.raw_execute("TRUNCATE phase8_t8_7_evt_row", &[])
        .await
        .expect("truncate phase8_t8_7_evt_row");
    ctx.raw_execute("TRUNCATE phase8_t8_7_evt_row_outbox", &[])
        .await
        .expect("truncate phase8_t8_7_evt_row_outbox");
}

// ── Fixture model 2 — non-events, backward-compat sentinel ──────────────────
#[model(table = "phase8_t8_7_plain_row", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct PlainRow {
    pub label: String,
}

async fn setup_plain_row(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE IF NOT EXISTS phase8_t8_7_plain_row (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            label       TEXT        NOT NULL
        )",
        &[],
    )
    .await
    .expect("create phase8_t8_7_plain_row table");

    ctx.raw_execute("TRUNCATE phase8_t8_7_plain_row", &[])
        .await
        .expect("truncate phase8_t8_7_plain_row");
}

// ── Test 1 — outbox 'delete' row propagates to Punnu tombstone ──────────────
#[djogi::djogi_test]
async fn hard_delete_propagates_via_outbox_to_tombstone(mut ctx: djogi::DjogiContext) {
    setup_event_row(&mut ctx).await;
    let pool = ctx
        .pool()
        .expect("djogi_test context must have a pool")
        .clone();

    // Insert a row inside a transaction so emit_event's outbox-create
    // INSERT lands in the same atomic boundary. We don't assert on the
    // outbox-create row — only the outbox-delete row matters for T8.7.
    let row = djogi::transaction::atomic(&pool, |inner| {
        Box::pin(async move {
            EventRow::create(
                inner,
                EventRow {
                    id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
                    created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                    updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                    label: "to-be-deleted".to_string(),
                },
            )
            .await
        })
    })
    .await
    .expect("create EventRow");
    let row_id = row.id;

    let punnu = ctx
        .punnu::<EventRow>()
        .expect("punnu registered for EventRow via inventory boot hook");
    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    let handle = EventRow::objects().refresh_into(&punnu, pool.clone(), auth.clone());

    // First tick: full scan, watermark None, outbox watermark None.
    // The live row is applied; the outbox poll branch initialises the
    // per-fetcher watermark to `OffsetDateTime::now_utc()` so subsequent
    // ticks see only events after this checkpoint.
    let tick_1 = handle.update().await.expect("first tick must succeed");
    assert_eq!(
        tick_1.applied,
        1,
        "first tick must apply 1 live item; got {applied}",
        applied = tick_1.applied,
    );
    assert!(
        punnu.get(&row_id).is_some(),
        "Punnu must hold the live row after first tick"
    );

    // Hard-delete in a transaction so the parent commits the DELETE +
    // emit_event's outbox INSERT atomically. After this commits, the
    // outbox table holds (at minimum) one row with action='create' from
    // the create call above and one with action='delete' from this
    // delete.
    djogi::transaction::atomic(&pool, |inner| {
        Box::pin(async move { row.delete(inner).await })
    })
    .await
    .expect("delete EventRow + outbox emit");

    let outbox_delete_count: i64 = ctx
        .raw_scalar(
            "SELECT COUNT(*) FROM phase8_t8_7_evt_row_outbox WHERE action = 'delete'",
            &[],
        )
        .await
        .expect("count outbox delete rows");
    assert_eq!(
        outbox_delete_count, 1,
        "outbox must hold exactly one 'delete' row after hard-delete"
    );

    // Second tick: outbox watermark is non-None now (initialised on
    // first tick). The 'delete' row's `created_at` was set by
    // PostgreSQL's `now()` AFTER the first tick's wall-clock
    // initialisation (the delete happens after the first tick returns),
    // so the poll's `created_at >= $watermark` predicate captures it.
    // The fetcher decodes row_id → HeerId → T::Id (via TypeId-checked
    // transmute_copy in `cast_heerid_to_t_id`) and feeds it into
    // `DeltaResult.tombstones`. Punnu's `apply_delta` evicts the entry.
    let _tick_2 = handle.update().await.expect("second tick must succeed");

    assert!(
        punnu.get(&row_id).is_none(),
        "Punnu entry must be evicted via outbox tombstone after hard-delete; \
         present-ness here would mean either the outbox poll is gated off \
         incorrectly, or the HeerId → T::Id cast returned None despite the \
         TypeId gate"
    );
}

// ── Test 2 — non-events model: outbox path is gated off ─────────────────────
#[djogi::djogi_test]
async fn non_events_model_no_outbox_poll(mut ctx: djogi::DjogiContext) {
    setup_plain_row(&mut ctx).await;
    let pool = ctx
        .pool()
        .expect("djogi_test context must have a pool")
        .clone();

    let row = PlainRow::create(
        &mut ctx,
        PlainRow {
            id: <::djogi::types::HeerId as ::djogi::PrimaryKey>::sentinel(),
            created_at: ::djogi::types::DateTime::UNIX_EPOCH,
            updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
            label: "stays-live".to_string(),
        },
    )
    .await
    .expect("create PlainRow");
    let row_id = row.id;

    let punnu = ctx
        .punnu::<PlainRow>()
        .expect("punnu registered for PlainRow via inventory boot hook");
    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    let handle = PlainRow::objects().refresh_into(&punnu, pool.clone(), auth);

    let tick_1 = handle.update().await.expect("first tick must succeed");
    assert_eq!(tick_1.applied, 1);
    assert!(punnu.get(&row_id).is_some());

    // Run a second tick. With no outbox table for this model, the
    // outbox-poll branch is gated off (`T::descriptor().has_outbox`
    // is `false`); a regression that ran the poll anyway would error
    // on the missing `phase8_t8_7_plain_row_outbox` table.
    let _tick_2 = handle
        .update()
        .await
        .expect("second tick must succeed (no outbox poll for non-events models)");

    assert!(
        punnu.get(&row_id).is_some(),
        "non-events model: Punnu entry must remain live (no spurious tombstone)"
    );
}
