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
//! Outbox table schema mirrors Phase 4:
//! `(id BIGINT PK DEFAULT generate_id(), row_id BIGINT, action TEXT,
//! payload JSONB, created_at TIMESTAMPTZ DEFAULT now())`.

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

// Fixture 1b — events-enabled with the default PK (HeerIdDesc).
// Pinned because the gate previously only accepted ascending HeerId,
// so the recency-biased default-PK case never polled the outbox.
#[model(table = "phase8_t8_7_evt_desc_row", events)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct EventDescRow {
    pub label: String,
}

async fn setup_event_desc_row(ctx: &mut djogi::DjogiContext) {
    // No top-level `generate_id_desc()` function ships with HeerRanjId
    // 0.3.x — the desc form is composed via `heerid_to_desc(generate_id())`.
    // djogi's projection layer assumes the singleton helper exists in
    // production deployments; for this integration fixture we inline
    // the composition so the test runs against the live HeerRanjId
    // schema as installed.
    ctx.raw_execute(
        "CREATE TABLE IF NOT EXISTS phase8_t8_7_evt_desc_row (
            id          BIGINT      PRIMARY KEY DEFAULT heerid_to_desc(generate_id()),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            label       TEXT        NOT NULL
        )",
        &[],
    )
    .await
    .expect("create phase8_t8_7_evt_desc_row table");

    ctx.raw_execute(
        "CREATE TABLE IF NOT EXISTS phase8_t8_7_evt_desc_row_outbox (
            id         BIGINT      PRIMARY KEY DEFAULT generate_id(),
            row_id     BIGINT      NOT NULL,
            action     TEXT        NOT NULL,
            payload    JSONB       NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
        &[],
    )
    .await
    .expect("create phase8_t8_7_evt_desc_row_outbox table");

    ctx.raw_execute("TRUNCATE phase8_t8_7_evt_desc_row", &[])
        .await
        .expect("truncate phase8_t8_7_evt_desc_row");
    ctx.raw_execute("TRUNCATE phase8_t8_7_evt_desc_row_outbox", &[])
        .await
        .expect("truncate phase8_t8_7_evt_desc_row_outbox");
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

// Test 1b — same round-trip but for the default (HeerIdDesc) PK. Pins
// the gate fix in `t_id_decodes_from_outbox_bigint` plus the cast in
// `cast_row_id_to_t_id`; without either, this model's tombstones would
// never propagate and the cache entry would survive.
#[djogi::djogi_test]
async fn hard_delete_propagates_for_default_heerid_desc_pk(mut ctx: djogi::DjogiContext) {
    setup_event_desc_row(&mut ctx).await;
    let pool = ctx
        .pool()
        .expect("djogi_test context must have a pool")
        .clone();

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
