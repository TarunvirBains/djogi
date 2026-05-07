//! Outbox-derived tombstones (Pattern 2) in the delta-sync fetcher.
//!
//! `hard_delete_propagates_via_outbox_to_tombstone` walks the round-trip:
//! a `#[model(events)]` model emits `action='delete'` into
//! `<table>_outbox` on hard delete, the fetcher's per-tick poll picks
//! it up, decodes `row_id BIGINT → HeerId → T::Id`, and Punnu's
//! `apply_delta` evicts the entry.
//!
//! `non_events_model_no_outbox_poll` checks the gate — a model without
//! `events` has no outbox table, so a regression that ran the poll
//! unconditionally would fail on the missing relation.
//!
//! Tables are provisioned via `#[djogi_test(sync_models = [...])]`.
//! For `#[model(events)]` fixtures, `sync_models` must synthesize the
//! framework-owned `<table>_outbox` companion table.

use djogi::prelude::*;

// ── Fixture model 1 — events-enabled, hard-delete tombstone source ──────────
#[model(table = "phase8_t8_7_evt_row", pk = HeerId, events)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct EventRow {
    pub label: String,
}

// Fixture 1b — events-enabled with the default PK (HeerIdDesc).
// Pinned because the gate previously only accepted ascending HeerId,
// so the recency-biased default-PK case never polled the outbox.
#[model(table = "phase8_t8_7_evt_desc_row", events)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct EventDescRow {
    pub label: String,
}

// ── Fixture model 2 — non-events, backward-compat sentinel ──────────────────
#[model(table = "phase8_t8_7_plain_row", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct PlainRow {
    pub label: String,
}

// ── Test 1 — outbox 'delete' row propagates to Punnu tombstone ──────────────
#[djogi::djogi_test(sync_models = [EventRow])]
async fn hard_delete_propagates_via_outbox_to_tombstone(mut ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");

    // Inside a transaction so the outbox-create INSERT lands atomically
    // with the row insert. Only the outbox-delete row matters for the
    // tombstone path.
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

    // First tick initialises the outbox watermark to `now()`; later
    // ticks only see events past that checkpoint.
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

    // Atomic delete + outbox INSERT.
    djogi::transaction::atomic(&pool, |inner| {
        Box::pin(async move { row.delete(inner).await })
    })
    .await
    .expect("delete EventRow + outbox emit");

    let outbox_rows = djogi::testing::outbox_rows_for_test(&mut ctx, "phase8_t8_7_evt_row_outbox")
        .await
        .expect("read EventRow outbox rows");
    let outbox_delete_count = outbox_rows
        .iter()
        .filter(|row| row.action == "delete")
        .count();
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

// Test 1b — same round-trip but for the default (HeerIdDesc) PK. Pins
// the gate fix in `t_id_decodes_from_outbox_bigint` plus the cast in
// `cast_row_id_to_t_id`; without either, this model's tombstones would
// never propagate and the cache entry would survive.
#[djogi::djogi_test(sync_models = [EventDescRow])]
async fn hard_delete_propagates_for_default_heerid_desc_pk(mut ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");

    let row = djogi::transaction::atomic(&pool, |inner| {
        Box::pin(async move {
            EventDescRow::create(
                inner,
                EventDescRow {
                    id: <::djogi::types::HeerIdDesc as ::djogi::PrimaryKey>::sentinel(),
                    created_at: ::djogi::types::DateTime::UNIX_EPOCH,
                    updated_at: ::djogi::types::DateTime::UNIX_EPOCH,
                    label: "desc-to-be-deleted".to_string(),
                },
            )
            .await
        })
    })
    .await
    .expect("create EventDescRow");
    let row_id = row.id;

    let punnu = ctx
        .punnu::<EventDescRow>()
        .expect("punnu registered for EventDescRow via inventory boot hook");
    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    let handle = EventDescRow::objects().refresh_into(&punnu, pool.clone(), auth.clone());

    let tick_1 = handle.update().await.expect("first tick must succeed");
    assert_eq!(tick_1.applied, 1);
    assert!(punnu.get(&row_id).is_some());

    djogi::transaction::atomic(&pool, |inner| {
        Box::pin(async move { row.delete(inner).await })
    })
    .await
    .expect("delete EventDescRow + outbox emit");

    let _tick_2 = handle.update().await.expect("second tick must succeed");

    assert!(
        punnu.get(&row_id).is_none(),
        "default-PK (HeerIdDesc) events models must propagate outbox tombstones; \
         a remaining entry here means the gate excluded HeerIdDesc or the \
         row_id → HeerIdDesc cast dropped the value"
    );
}

// ── Test 2 — non-events model: outbox path is gated off ─────────────────────
#[djogi::djogi_test(sync_models = [PlainRow])]
async fn non_events_model_no_outbox_poll(mut ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");

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
