// Live Postgres checks for `PairWindowExt::*_pair_expr`
// methods (GH #302).
//
// Verifies that:
//
//  1. `order_by_pair_expr_desc` ranks self-pairs by a scaled scalar expression
//     (`l.score * 10`), producing row numbers that reflect the left-side score
//     ordering under the left alias.
//
//  2. `partition_by_pair_expr` combined with `order_by_pair_expr_desc` resets
//     rank numbering per group (expressed via `l.group_id * 1` and
//     `l.score * 1` identity-multiplication expressions to exercise the
//     `WindowTerm::Expr` code path rather than the bare-column path).
//
// Both tests exercise the `Expr`-backed pair window path introduced in GH #302
// and confirm the `is_joined_safe` contract: window functions whose spec
// contains only `WindowTerm::Expr` entries are pair-qualified by construction
// and pass the joined-annotation safety gate.

use djogi::prelude::*;

// Table name is unique to this fixture — avoids collisions with other tests
// that share the Postgres instance.
#[model(table = "pair_window_expr_items")]
#[derive(Debug, Clone)]
pub struct Item {
    pub name: String,
    pub score: i32,
    pub group_id: i32,
}

/// Verify that `order_by_pair_expr_desc` ranks self-pairs by a scaled score.
///
/// Seeds three items (alpha=10, beta=20, gamma=30) all in group 1. The window
/// function is `ROW_NUMBER() OVER (ORDER BY l.score * 10 DESC)`. Gamma has the
/// highest scaled score (300), so all pairs where the left member is gamma
/// must have `row_number ≤ 3` — only two other items exist, so three is the
/// maximum row number any item can ever reach in the ordering across all pairs
/// sharing the same left item.
#[djogi::djogi_test(sync_models = [Item])]
async fn order_by_pair_expr_desc_ranks_by_scaled_score(mut ctx: djogi::DjogiContext) {
    Item::create(
        &mut ctx,
        Item {
            name: "alpha".to_string(),
            score: 10,
            group_id: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    Item::create(
        &mut ctx,
        Item {
            name: "beta".to_string(),
            score: 20,
            group_id: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    Item::create(
        &mut ctx,
        Item {
            name: "gamma".to_string(),
            score: 30,
            group_id: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // `self_pairs()` excludes the identity row (l.id <> r.id), so with 3 rows
    // the cross-join produces 6 ordered pairs.
    let results: Vec<((Item, Item), i64)> = Item::objects()
        .self_pairs()
        .annotate(|lf, _rf| {
            // Pair-expr ordering: ROW_NUMBER() OVER (ORDER BY l.score * 10 DESC)
            // Using `order_by_pair_expr_desc` produces a `WindowTerm::Expr`
            // entry qualified under the left alias, which the joined-safe gate
            // accepts. The expression `l.score * 10` scales the raw score so
            // the test exercises the full arithmetic pipeline, not a no-op.
            RowNumber::new()
                .order_by_pair_expr_desc(
                    PairSide::Left,
                    lf.score().as_expr() * Expr::literal(10i32),
                )
                .alias("rn")
        })
        .fetch_all(&mut ctx)
        .await
        .unwrap();

    assert!(!results.is_empty(), "expected at least one pair");

    // All pairs where the left item is "gamma" (scaled score = 300, highest)
    // must have row_number ≤ 3 — there are only 3 items so the row number
    // for any left item never exceeds 3 across the global ordering.
    let gamma_pairs: Vec<_> = results
        .iter()
        .filter(|((l, _r), _rn)| l.name == "gamma")
        .collect();

    assert!(
        !gamma_pairs.is_empty(),
        "expected at least one pair with left='gamma'"
    );

    for ((l, _r), rn) in &gamma_pairs {
        assert_eq!(l.name, "gamma");
        assert!(
            *rn <= 3,
            "gamma has highest scaled score (300); all its pairs should rank \
             in top 3, got row_number={rn}"
        );
    }
}

/// Verify that `partition_by_pair_expr` resets rank numbering per group.
///
/// Seeds four items across two groups (group 1: scores 100 and 200; group 2:
/// scores 50 and 75). The window function is:
///
/// ```text
/// RANK() OVER (PARTITION BY l.group_id * 1 ORDER BY l.score * 1 DESC)
/// ```
///
/// Within each partition the highest scorer should receive `rank = 1`.
/// Identity multiplication (`* 1`) is used deliberately — it exercises the
/// `WindowTerm::Expr` code path rather than the bare-column `WindowTerm::Column`
/// path, which is what GH #302 added.
#[djogi::djogi_test(sync_models = [Item])]
async fn partition_by_pair_expr_resets_rank_per_group(mut ctx: djogi::DjogiContext) {
    Item::create(
        &mut ctx,
        Item {
            name: "g1_a".to_string(),
            score: 100,
            group_id: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    Item::create(
        &mut ctx,
        Item {
            name: "g1_b".to_string(),
            score: 200,
            group_id: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    Item::create(
        &mut ctx,
        Item {
            name: "g2_a".to_string(),
            score: 50,
            group_id: 2,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    Item::create(
        &mut ctx,
        Item {
            name: "g2_b".to_string(),
            score: 75,
            group_id: 2,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let results: Vec<((Item, Item), i64)> = Item::objects()
        .self_pairs()
        .annotate(|lf, _rf| {
            // Pair-expr partition + ordering:
            //   RANK() OVER (
            //     PARTITION BY l.group_id * 1   ← resets rank per group
            //     ORDER BY l.score * 1 DESC     ← highest score → rank 1
            //   )
            // Both expressions use `* 1` so the ExprNode path (not the
            // bare-column path) is exercised while the semantic result is
            // identical to a bare field reference.
            Rank::new()
                .partition_by_pair_expr(
                    PairSide::Left,
                    lf.group_id().as_expr() * Expr::literal(1i32),
                )
                .order_by_pair_expr_desc(PairSide::Left, lf.score().as_expr() * Expr::literal(1i32))
                .alias("rank")
        })
        .fetch_all(&mut ctx)
        .await
        .unwrap();

    assert!(!results.is_empty(), "expected at least one pair");

    // Within group 1, g1_b (score = 200) is the highest scorer → rank = 1.
    let g1_b_pairs: Vec<_> = results
        .iter()
        .filter(|((l, _r), _rank)| l.name == "g1_b")
        .collect();
    assert!(
        !g1_b_pairs.is_empty(),
        "expected at least one pair with left='g1_b'"
    );
    for ((l, _r), rank) in &g1_b_pairs {
        assert_eq!(l.name, "g1_b");
        assert_eq!(
            *rank, 1,
            "g1_b has highest score in group 1; expected rank=1, got rank={rank}"
        );
    }

    // Within group 2, g2_b (score = 75) is the highest scorer → rank = 1.
    let g2_b_pairs: Vec<_> = results
        .iter()
        .filter(|((l, _r), _rank)| l.name == "g2_b")
        .collect();
    assert!(
        !g2_b_pairs.is_empty(),
        "expected at least one pair with left='g2_b'"
    );
    for ((l, _r), rank) in &g2_b_pairs {
        assert_eq!(l.name, "g2_b");
        assert_eq!(
            *rank, 1,
            "g2_b has highest score in group 2; expected rank=1, got rank={rank}"
        );
    }
}
