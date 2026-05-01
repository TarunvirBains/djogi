//! Phase 8-Zero Cluster C C3 — live-Postgres checks for typed window-only
//! annotations and derived-table filtering.

use djogi::prelude::*;

#[model(table = "window_elephants_p8c", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct WindowElephant {
    pub herd_id: i64,
    pub score: i64,
    pub label: String,
}

async fn setup_window_elephants(ctx: &mut djogi::DjogiContext) {
    ctx.raw_ddl(
        "CREATE TABLE IF NOT EXISTS window_elephants_p8c (
             id         BIGINT       PRIMARY KEY DEFAULT generate_id(),
             created_at TIMESTAMPTZ  NOT NULL    DEFAULT now(),
             updated_at TIMESTAMPTZ  NOT NULL    DEFAULT now(),
             herd_id    BIGINT       NOT NULL,
             score      BIGINT       NOT NULL,
             label      TEXT         NOT NULL
         );",
    )
    .await
    .expect("window elephant table DDL must succeed");
}

fn elephant(herd_id: i64, score: i64, label: &str) -> WindowElephant {
    WindowElephant {
        id: djogi::HeerId::from_i64(0).expect("HeerId sentinel"),
        created_at: djogi::DateTime::UNIX_EPOCH,
        updated_at: djogi::DateTime::UNIX_EPOCH,
        herd_id,
        score,
        label: label.to_owned(),
    }
}

async fn seed_elephants(ctx: &mut djogi::DjogiContext, rows: &[(i64, i64, &str)]) {
    for (herd_id, score, label) in rows {
        WindowElephant::create(ctx, elephant(*herd_id, *score, label))
            .await
            .expect("create window elephant");
    }
}

#[djogi::djogi_test]
async fn row_number_qualify_returns_top_three_per_herd(mut ctx: djogi::DjogiContext) {
    setup_window_elephants(&mut ctx).await;
    seed_elephants(
        &mut ctx,
        &[
            (1, 100, "a1"),
            (1, 90, "a2"),
            (1, 80, "a3"),
            (1, 70, "a4"),
            (2, 50, "b1"),
            (2, 40, "b2"),
            (2, 30, "b3"),
            (2, 20, "b4"),
        ],
    )
    .await;

    let mut rows: Vec<(WindowElephant, i64)> = WindowElephant::objects()
        .annotate(|e| {
            RowNumber::new()
                .partition_by(e.herd_id())
                .order_by(e.score().desc())
                .alias("rank")
        })
        .qualify(|w| w.lte(3))
        .fetch_all(&mut ctx)
        .await
        .expect("row_number qualify query must execute");

    rows.sort_by_key(|(elephant, rank)| (elephant.herd_id, *rank));

    assert_eq!(rows.len(), 6, "expected top three rows per herd");
    for herd_id in [1_i64, 2_i64] {
        let ranks: Vec<i64> = rows
            .iter()
            .filter(|(elephant, _)| elephant.herd_id == herd_id)
            .map(|(_, rank)| *rank)
            .collect();
        assert_eq!(ranks, vec![1, 2, 3], "herd {herd_id} ranks");
    }
}

#[djogi::djogi_test]
async fn rank_and_dense_rank_handle_ties_differently(mut ctx: djogi::DjogiContext) {
    setup_window_elephants(&mut ctx).await;
    seed_elephants(
        &mut ctx,
        &[(9, 100, "tie-a"), (9, 100, "tie-b"), (9, 90, "next")],
    )
    .await;

    let mut ranks: Vec<(WindowElephant, i64)> = WindowElephant::objects()
        .annotate(|e| {
            Rank::new()
                .partition_by(e.herd_id())
                .order_by(e.score().desc())
                .alias("rank")
        })
        .fetch_all(&mut ctx)
        .await
        .expect("rank query must execute");
    ranks.sort_by_key(|(elephant, rank)| (-elephant.score, *rank, elephant.label.clone()));
    let rank_values: Vec<i64> = ranks.iter().map(|(_, rank)| *rank).collect();

    let mut dense_ranks: Vec<(WindowElephant, i64)> = WindowElephant::objects()
        .annotate(|e| {
            DenseRank::new()
                .partition_by(e.herd_id())
                .order_by(e.score().desc())
                .alias("dense_rank")
        })
        .fetch_all(&mut ctx)
        .await
        .expect("dense_rank query must execute");
    dense_ranks.sort_by_key(|(elephant, rank)| (-elephant.score, *rank, elephant.label.clone()));
    let dense_rank_values: Vec<i64> = dense_ranks.iter().map(|(_, rank)| *rank).collect();

    assert_eq!(rank_values, vec![1, 1, 3]);
    assert_eq!(dense_rank_values, vec![1, 1, 2]);
}
