//! Phase 7-Zero-2 T5 live coverage for `bulk_create` pre-allocation.
//!
//! The post-T5 emission dispatches on `pk_kind`:
//!
//! - `HeerId` / `HeerIdDesc` / `RanjId` / `RanjIdDesc` / custom DB-gen:
//!   pre-allocate `N` ids in one round-trip through
//!   `PrimaryKeyDbGen::generate_many(ctx, n)`, then `INSERT` with
//!   explicit id values — no per-row column `DEFAULT` fires.
//! - `Serial`: no `PrimaryKeyDbGen` impl exists; keep the per-row
//!   `DEFAULT` path (exercised by other Phase 1 / Phase 4 fixtures, not
//!   retested here).
//!
//! The sharpest in-Rust witness that the new path runs is "the INSERT
//! binds the id column explicitly": create the backing table **without**
//! a `DEFAULT` on `id`, then call `bulk_create`. Under the pre-T5
//! per-row-`DEFAULT` emission the INSERT omitted the `id` column
//! altogether, so Postgres would fail with a `NOT NULL` violation;
//! under the post-T5 dispatch the caller's pre-allocated ids are bound
//! positionally and the INSERT succeeds.
//!
//! Ascending / descending / uniqueness assertions layer additional
//! sanity checks on top — they witness the batch ordering contract
//! `PrimaryKeyDbGen::generate_many` upholds.

use djogi::prelude::*;
use djogi::types::{HeerId, HeerIdRecencyBiased, RanjId, RanjIdRecencyBiased};

// ── HeerId — ascending, one round-trip ────────────────────────────────

#[model(table = "phase7_zero2_t5_bulk_asc", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct AscRow {
    pub name: String,
}

async fn setup_asc(ctx: &mut DjogiContext) {
    // No `DEFAULT` on `id` — the post-T5 `bulk_create` must bind `id`
    // explicitly from its pre-allocated batch.
    ctx.raw_execute(
        "CREATE TABLE phase7_zero2_t5_bulk_asc (
            id          BIGINT      PRIMARY KEY,
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            name        TEXT        NOT NULL
         )",
        &[],
    )
    .await
    .expect("CREATE TABLE must succeed");
}

#[djogi::djogi_test]
async fn heerid_bulk_create_pre_allocates_and_preserves_ascending_order(mut ctx: DjogiContext) {
    setup_asc(&mut ctx).await;

    let rows: Vec<AscRow> = (0..5)
        .map(|i| AscRow {
            name: format!("row-{i}"),
            ..Default::default()
        })
        .collect();

    let created = AscRow::bulk_create(&mut ctx, rows).await.expect(
        "bulk_create must succeed on a table without a DEFAULT on id — \
             the pre-T5 per-row DEFAULT path would fail with a NOT NULL violation",
    );

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

async fn setup_desc(ctx: &mut DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE phase7_zero2_t5_bulk_desc (
            id          BIGINT      PRIMARY KEY,
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            name        TEXT        NOT NULL
         )",
        &[],
    )
    .await
    .expect("CREATE TABLE must succeed");
}

#[djogi::djogi_test]
async fn heerid_desc_bulk_create_pre_allocates_and_preserves_descending_order(
    mut ctx: DjogiContext,
) {
    setup_desc(&mut ctx).await;

    let rows: Vec<DescRow> = (0..5)
        .map(|i| DescRow {
            name: format!("row-{i}"),
            ..Default::default()
        })
        .collect();

    let created = DescRow::bulk_create(&mut ctx, rows)
        .await
        .expect("bulk_create must succeed on a table without a DEFAULT on id");

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

async fn setup_ranj(ctx: &mut DjogiContext) {
    // `generate_ranjids(...)` reads `current_heer_ranj_node_id()`, a
    // separate GUC from `heer.node_id`. The `#[djogi_test]` harness
    // only seeds the latter; seed the former explicitly.
    ctx.raw_execute("SELECT set_heer_ranj_node_id(1)", &[])
        .await
        .expect("set_heer_ranj_node_id(1) must succeed");
    ctx.raw_execute(
        "CREATE TABLE phase7_zero2_t5_bulk_ranj (
            id          UUID        PRIMARY KEY,
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            name        TEXT        NOT NULL
         )",
        &[],
    )
    .await
    .expect("CREATE TABLE must succeed");
}

#[djogi::djogi_test]
async fn ranjid_bulk_create_pre_allocates_distinct_non_sentinel_ids(mut ctx: DjogiContext) {
    setup_ranj(&mut ctx).await;

    let rows: Vec<RanjRow> = (0..4)
        .map(|i| RanjRow {
            name: format!("row-{i}"),
            ..Default::default()
        })
        .collect();

    let created = RanjRow::bulk_create(&mut ctx, rows)
        .await
        .expect("bulk_create must succeed on a table without a DEFAULT on id");

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

async fn setup_ranj_desc(ctx: &mut DjogiContext) {
    ctx.raw_execute("SELECT set_heer_ranj_node_id(1)", &[])
        .await
        .expect("set_heer_ranj_node_id(1) must succeed");
    ctx.raw_execute(
        "CREATE TABLE phase7_zero2_t5_bulk_ranj_desc (
            id          UUID        PRIMARY KEY,
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            name        TEXT        NOT NULL
         )",
        &[],
    )
    .await
    .expect("CREATE TABLE must succeed");
}

#[djogi::djogi_test]
async fn ranjid_desc_bulk_create_pre_allocates_distinct_non_sentinel_ids(mut ctx: DjogiContext) {
    setup_ranj_desc(&mut ctx).await;

    let rows: Vec<RanjDescRow> = (0..4)
        .map(|i| RanjDescRow {
            name: format!("row-{i}"),
            ..Default::default()
        })
        .collect();

    let created = RanjDescRow::bulk_create(&mut ctx, rows)
        .await
        .expect("bulk_create must succeed on a table without a DEFAULT on id");

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

// ── Custom PK backed by a sequence (DB-gen via bulk_sql) ──────────────
//
// Exercises the `djogi::primary_key! { ... bulk_sql = "..." }` branch:
// the macro emits `PrimaryKeyDbGen` whose `generate_many` runs the
// supplied SQL with the batch count as `$1`. The `bulk_create`
// dispatch routes custom DB-gen PKs down the same pre-allocation path
// as the built-ins.

djogi::primary_key! {
    pub struct CustomBulkId(i64);
    sql_type = "BIGINT";
    default_sql = "nextval('phase7_zero2_t5_custom_seq')";
    bulk_sql = "SELECT nextval('phase7_zero2_t5_custom_seq') AS id \
                FROM generate_series(1, $1)";
}

#[model(table = "phase7_zero2_t5_bulk_custom", pk = CustomBulkId)]
#[derive(Debug, Clone)]
pub struct CustomRow {
    pub name: String,
}

async fn setup_custom(ctx: &mut DjogiContext) {
    ctx.raw_execute("CREATE SEQUENCE phase7_zero2_t5_custom_seq START 1", &[])
        .await
        .expect("CREATE SEQUENCE must succeed");
    ctx.raw_execute(
        "CREATE TABLE phase7_zero2_t5_bulk_custom (
            id          BIGINT      PRIMARY KEY,
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            name        TEXT        NOT NULL
         )",
        &[],
    )
    .await
    .expect("CREATE TABLE must succeed");
}

#[djogi::djogi_test]
async fn custom_db_gen_pk_bulk_create_pre_allocates_ascending_ids(mut ctx: DjogiContext) {
    setup_custom(&mut ctx).await;

    let rows: Vec<CustomRow> = (0..5)
        .map(|i| CustomRow {
            name: format!("row-{i}"),
            ..Default::default()
        })
        .collect();

    let created = CustomRow::bulk_create(&mut ctx, rows)
        .await
        .expect("bulk_create must succeed for a custom DB-gen PK without a DEFAULT on id");

    assert_eq!(created.len(), 5);
    let sentinel = <CustomBulkId as PrimaryKey>::sentinel();
    for row in &created {
        assert_ne!(
            row.id, sentinel,
            "bulk_create left the sentinel id in place"
        );
    }
    // A plain `CREATE SEQUENCE` hands out strictly ascending bigints
    // per `nextval` call. The `djogi::primary_key!` macro does not
    // derive `Ord` / `PartialOrd` on custom newtypes, so compare the
    // inner `i64` directly via the `pub` tuple field.
    for win in created.windows(2) {
        assert!(
            win[0].id.0 < win[1].id.0,
            "custom DB-gen PK bulk_create ids not strictly ascending: {:?}",
            (win[0].id, win[1].id),
        );
    }
}

// ── Empty batch short-circuit ─────────────────────────────────────────

#[djogi::djogi_test]
async fn heerid_bulk_create_empty_batch_is_a_noop(mut ctx: DjogiContext) {
    setup_asc(&mut ctx).await;

    let created = AscRow::bulk_create(&mut ctx, Vec::new())
        .await
        .expect("empty bulk_create must succeed without touching the DB");

    assert!(
        created.is_empty(),
        "empty bulk_create returned rows: {created:?}",
    );
}
