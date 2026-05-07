// Phase 7-Zero-2 T5 live coverage for `bulk_create` pre-allocation.
//
// The post-T5 emission dispatches on `pk_kind`:
//
// - `HeerId` / `HeerIdDesc` / `RanjId` / `RanjIdDesc` / custom DB-gen:
//   pre-allocate `N` ids in one round-trip through
//   `PrimaryKeyDbGen::generate_many(ctx, n)`, then `INSERT` with
//   explicit id values — no per-row column `DEFAULT` fires.
// - `Serial`: no `PrimaryKeyDbGen` impl exists; keep the per-row
//   `DEFAULT` path (exercised by other Phase 1 / Phase 4 fixtures, not
//   retested here).
//
// The ordinary-surface witness is behavioral: `bulk_create` returns
// non-sentinel ids with the ordering/distinctness guarantees that
// `PrimaryKeyDbGen::generate_many` upholds. Schema setup is handled by
// `sync_models`.

use djogi::prelude::*;
use djogi::types::{HeerId, HeerIdRecencyBiased, RanjId, RanjIdRecencyBiased};

// ── HeerId — ascending, one round-trip ────────────────────────────────

#[model(table = "phase7_zero2_t5_bulk_asc", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct AscRow {
    pub name: String,
}

#[djogi::djogi_test(sync_models = [AscRow])]
async fn heerid_bulk_create_pre_allocates_and_preserves_ascending_order(mut ctx: DjogiContext) {
    let rows: Vec<AscRow> = (0..5)
        .map(|i| AscRow {
            name: format!("row-{i}"),
            ..Default::default()
        })
        .collect();

    let created = AscRow::bulk_create(&mut ctx, rows)
        .await
        .expect("bulk_create must succeed");

    assert_eq!(created.len(), 5);
    let sentinel = <HeerId as PrimaryKey>::sentinel();
    for row in &created {
        assert_ne!(
            row.id, sentinel,
            "bulk_create left the sentinel id in place"
        );
    }
    // `generate_many` returns ids in monotonic key order for ascending
    // variants — the batch is therefore strictly ascending.
    for win in created.windows(2) {
        assert!(
            win[0].id < win[1].id,
            "HeerId bulk_create ids not strictly ascending: {:?}",
            (win[0].id, win[1].id),
        );
    }
}

// ── HeerIdRecencyBiased — descending, one round-trip ──────────────────

#[model(table = "phase7_zero2_t5_bulk_desc", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct DescRow {
    pub name: String,
}

#[djogi::djogi_test(sync_models = [DescRow])]
async fn heerid_desc_bulk_create_pre_allocates_and_preserves_descending_order(
    mut ctx: DjogiContext,
) {
    let rows: Vec<DescRow> = (0..5)
        .map(|i| DescRow {
            name: format!("row-{i}"),
            ..Default::default()
        })
        .collect();

    let created = DescRow::bulk_create(&mut ctx, rows)
        .await
        .expect("bulk_create must succeed");

    assert_eq!(created.len(), 5);
    let sentinel = <HeerIdRecencyBiased as PrimaryKey>::sentinel();
    for row in &created {
        assert_ne!(
            row.id, sentinel,
            "bulk_create left the sentinel id in place"
        );
    }
    // Desc variants encode recency as a descending bit pattern — the
    // newest allocation sorts smallest under the default `Ord` impl.
    // The pre-allocated batch therefore reads strictly decreasing.
    for win in created.windows(2) {
        assert!(
            win[0].id > win[1].id,
            "HeerIdRecencyBiased bulk_create ids not strictly descending: {:?}",
            (win[0].id, win[1].id),
        );
    }
}

// ── RanjId — distinct ids, one round-trip ─────────────────────────────

#[model(table = "phase7_zero2_t5_bulk_ranj", pk = RanjId)]
#[derive(Debug, Clone)]
pub struct RanjRow {
    pub name: String,
}

#[djogi::djogi_test(sync_models = [RanjRow])]
async fn ranjid_bulk_create_pre_allocates_distinct_non_sentinel_ids(mut ctx: DjogiContext) {
    let rows: Vec<RanjRow> = (0..4)
        .map(|i| RanjRow {
            name: format!("row-{i}"),
            ..Default::default()
        })
        .collect();

    let created = RanjRow::bulk_create(&mut ctx, rows)
        .await
        .expect("bulk_create must succeed");

    assert_eq!(created.len(), 4);
    let sentinel = <RanjId as PrimaryKey>::sentinel();
    for row in &created {
        assert_ne!(
            row.id, sentinel,
            "bulk_create left the sentinel id in place"
        );
    }
    let mut ids: Vec<RanjId> = created.iter().map(|r| r.id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 4, "bulk_create emitted duplicate RanjId values");
}

// ── RanjIdRecencyBiased — distinct ids, one round-trip ────────────────

#[model(table = "phase7_zero2_t5_bulk_ranj_desc", pk = RanjIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct RanjDescRow {
    pub name: String,
}

#[djogi::djogi_test(sync_models = [RanjDescRow])]
async fn ranjid_desc_bulk_create_pre_allocates_distinct_non_sentinel_ids(mut ctx: DjogiContext) {
    let rows: Vec<RanjDescRow> = (0..4)
        .map(|i| RanjDescRow {
            name: format!("row-{i}"),
            ..Default::default()
        })
        .collect();

    let created = RanjDescRow::bulk_create(&mut ctx, rows)
        .await
        .expect("bulk_create must succeed");

    assert_eq!(created.len(), 4);
    let sentinel = <RanjIdRecencyBiased as PrimaryKey>::sentinel();
    for row in &created {
        assert_ne!(
            row.id, sentinel,
            "bulk_create left the sentinel id in place"
        );
    }
    let mut ids: Vec<RanjIdRecencyBiased> = created.iter().map(|r| r.id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(
        ids.len(),
        4,
        "bulk_create emitted duplicate RanjIdRecencyBiased values",
    );
}

// ── Empty batch short-circuit ─────────────────────────────────────────

#[djogi::djogi_test(sync_models = [AscRow])]
async fn heerid_bulk_create_empty_batch_is_a_noop(mut ctx: DjogiContext) {
    let created = AscRow::bulk_create(&mut ctx, Vec::new())
        .await
        .expect("empty bulk_create must succeed without touching the DB");

    assert!(
        created.is_empty(),
        "empty bulk_create returned rows: {created:?}",
    );
}
