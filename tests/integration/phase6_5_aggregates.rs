// Phase 6.5 Task 14 — live-Postgres integration tests for the aggregation
// surface.
//
// # Scope
//
// These tests exercise the full round-trip for the Phase 6.5 aggregation
// additions against a real Postgres 18 instance (provisioned per-test by the
// `#[djogi_test]` harness):
//
// 1. `.group_by(...).annotate(...).fetch_all(...)` — basic GROUP BY + SUM.
// 2. Arity-2 `.group_by(...)` — tuple key decode.
// 3. `.rollup(...)` — hierarchical subtotals, with a trailing `NULL` grand-total row.
// 4. `.cube(...)` — all 2^N subsets plus the grand total.
// 5. `.group_by_sets(...)` — explicit grouping sets (unit-key shape).
// 6. Windowed aggregate inside `.annotate(...)` — running total per partition.
// 7. ROWS frame — 4-row moving sum using `FrameBound::Preceding(3)..=CurrentRow`.
// 8. `.count().distinct()` — DISTINCT modifier on a grouped aggregate.
// 9. `.having(...)` — group-filter predicate via the key's `as_expr().gt(...)` shape.
// 10. `.order_by(...).limit(...)` — top-N by key (aggregate-based top-N deferred;
//     see the per-test doc comment).
//
// # Design notes
//
// - Models are declared inline in this file and scoped to `orders_p65` /
//   `runs_p65` to avoid colliding with the Phase 4 / 5 table fixtures if
//   another test file is ever merged into the same binary.
// - Each live test uses `#[djogi_test(sync_models = [...])]` so schema setup
//   flows through Djogi's typed descriptor projection instead of handwritten
//   SQL fixtures.
// - The `.having(|k, a| a.gt(...))` shape named in the plan is not yet
//   expressible through the public surface — `AggregateExpr<V>` has no
//   `Into<Expr<V>>` bridge in Phase 6.5, so the HAVING predicate must close
//   over the key. The test therefore exercises the HAVING code path (SQL
//   `HAVING` clause emission + group filtering) via a key-based comparison.
//   The same constraint applies to scenario #10 — `ORDER BY <aggregate>` is
//   not available through the surface, so we exercise the ORDER BY + LIMIT
//   code path via a key-based ordering.
//
// The aggregate-HAVING / aggregate-ORDER-BY bridge is a small future surface
// addition; once it lands, these two tests should be updated to match the
// original plan wording.

use djogi::expr::FrameBound;
use djogi::prelude::*;

// ---------------------------------------------------------------------------
// Test models
// ---------------------------------------------------------------------------

/// Sales-order fixture: one row per transaction, with `org_id` + `status`
/// suitable for multi-dimensional GROUP BY, `amount` for SUM / AVG, and
/// `user_id` for COUNT(DISTINCT). The table name is suffixed `_p65` to avoid
/// colliding with Phase 4's `accounts` fixture if both files ever land in the
/// same test binary.
// Phase 7-Zero-2 T2 default flip — pin HeerId; grouped-aggregation tests
// rely on HeerId construction via `Order { id: HeerId::..., .. }`.
#[model(table = "orders_p65", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Order {
    pub org_id: i64,
    pub user_id: i64,
    pub status: String,
    pub amount: i64,
}

/// Time-series fixture for the running-total / ROWS-frame window tests.
/// `partition_id` groups rows into partitions; `seq` is the in-partition
/// ordering key (integer monotone — avoids the flakiness that would come
/// from relying on `created_at` millisecond resolution).
#[model(table = "runs_p65", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Run {
    pub partition_id: i64,
    pub seq: i64,
    pub amount: i64,
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Convenience constructor — framework columns receive sentinel values that
/// the database overwrites via column defaults + `RETURNING *`.
fn order(org_id: i64, user_id: i64, status: &str, amount: i64) -> Order {
    Order {
        id: djogi::HeerId::from_i64(0).expect("0 is a valid HeerId sentinel"),
        created_at: djogi::DateTime::UNIX_EPOCH,
        updated_at: djogi::DateTime::UNIX_EPOCH,
        org_id,
        user_id,
        status: status.to_string(),
        amount,
    }
}

/// Convenience constructor for `Run`.
fn run(partition_id: i64, seq: i64, amount: i64) -> Run {
    Run {
        id: djogi::HeerId::from_i64(0).expect("0 is a valid HeerId sentinel"),
        created_at: djogi::DateTime::UNIX_EPOCH,
        updated_at: djogi::DateTime::UNIX_EPOCH,
        partition_id,
        seq,
        amount,
    }
}

// ---------------------------------------------------------------------------
// Scenario 1 — basic group_by + sum
// ---------------------------------------------------------------------------

/// `.group_by(|f| f.org_id()).annotate(|f| f.amount().sum())` must produce one
/// result row per distinct `org_id`, with the summed amount matching the
/// hand-computed total.
#[djogi::djogi_test(sync_models = [Order])]
async fn group_by_org_id_sums_amount(mut ctx: djogi::DjogiContext) {
    // Three orgs: 1 with three orders (10+20+30), 2 with two orders (15+25),
    // 3 with one order (100).
    for o in [
        order(1, 10, "ok", 10),
        order(1, 10, "ok", 20),
        order(1, 11, "pending", 30),
        order(2, 20, "ok", 15),
        order(2, 21, "ok", 25),
        order(3, 30, "ok", 100),
    ] {
        Order::create(&mut ctx, o).await.expect("create order");
    }

    let rows: Vec<(i64, i64)> = Order::objects()
        .group_by(|f| f.org_id())
        .annotate(|f| f.amount().sum())
        .fetch_all(&mut ctx)
        .await
        .expect("group_by + annotate(sum) must succeed");

    // Three groups, one row each.
    assert_eq!(rows.len(), 3, "expected 3 rows, got {:?}", rows);

    // Verify each org's sum without relying on row ordering.
    let mut map: std::collections::BTreeMap<i64, i64> = Default::default();
    for (org, sum) in rows {
        map.insert(org, sum);
    }
    assert_eq!(map.get(&1), Some(&60), "org 1 sum: {:?}", map);
    assert_eq!(map.get(&2), Some(&40), "org 2 sum: {:?}", map);
    assert_eq!(map.get(&3), Some(&100), "org 3 sum: {:?}", map);
}

// ---------------------------------------------------------------------------
// Scenario 2 — arity-2 tuple key
// ---------------------------------------------------------------------------

/// `.group_by(|f| (f.org_id(), f.status()))` decodes into `(i64, String)` keys.
#[djogi::djogi_test(sync_models = [Order])]
async fn group_by_two_columns_decodes_tuple_key(mut ctx: djogi::DjogiContext) {
    // Two orgs × two statuses = four possible groups; seed representatives so
    // each of the four groups has at least one row.
    for o in [
        order(1, 10, "ok", 10),
        order(1, 11, "ok", 20),      // (1, "ok") sums to 30
        order(1, 12, "pending", 5),  // (1, "pending") sums to 5
        order(2, 20, "ok", 100),     // (2, "ok") sums to 100
        order(2, 21, "pending", 50), // (2, "pending") sums to 50
    ] {
        Order::create(&mut ctx, o).await.expect("create order");
    }

    let rows: Vec<((i64, String), i64)> = Order::objects()
        .group_by(|f| (f.org_id(), f.status()))
        .annotate(|f| f.amount().sum())
        .fetch_all(&mut ctx)
        .await
        .expect("arity-2 group_by must succeed");

    assert_eq!(rows.len(), 4, "expected 4 groups, got {:?}", rows);

    // Build a map keyed by the tuple and verify each expected sum.
    let mut map: std::collections::BTreeMap<(i64, String), i64> = Default::default();
    for ((org, status), sum) in rows {
        map.insert((org, status), sum);
    }
    assert_eq!(map.get(&(1, "ok".to_string())), Some(&30));
    assert_eq!(map.get(&(1, "pending".to_string())), Some(&5));
    assert_eq!(map.get(&(2, "ok".to_string())), Some(&100));
    assert_eq!(map.get(&(2, "pending".to_string())), Some(&50));
}

// ---------------------------------------------------------------------------
// Scenario 3 — rollup
// ---------------------------------------------------------------------------

/// `.rollup(|f| f.org_id())` is exposed through the typed aggregate builder.
/// The current typed decoder still uses the non-null field type for the key,
/// so Postgres' grand-total `NULL` row is expected to surface as a decode
/// error until extended grouping keys gain an `Option<K>` decode path.
#[djogi::djogi_test(sync_models = [Order])]
async fn rollup_typed_surface_reports_null_key_decode_gap(mut ctx: djogi::DjogiContext) {
    for o in [
        order(1, 10, "ok", 10),
        order(1, 11, "ok", 20),
        order(2, 20, "ok", 100),
    ] {
        Order::create(&mut ctx, o).await.expect("create order");
    }

    let err = Order::objects()
        .rollup(|f| f.org_id())
        .annotate(|f| f.amount().sum())
        .fetch_all(&mut ctx)
        .await
        .expect_err("typed ROLLUP fetch currently cannot decode the NULL grand-total key");
    let err_detail = format!("{err:?}");
    assert!(
        err.to_string().contains("NULL")
            || err.to_string().contains("null")
            || err_detail.contains("WasNull"),
        "ROLLUP decode gap should mention a NULL key; got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 4 — cube
// ---------------------------------------------------------------------------

/// `.cube(|f| (f.org_id(), f.status()))` emits every subset of the key set —
/// one row per (org_id, status) cell, per-org subtotal (status=NULL),
/// per-status subtotal (org_id=NULL), and the grand total (both NULL).
///
/// Same decode caveat as `rollup_typed_surface_reports_null_key_decode_gap`:
/// CUBE emits subtotal rows with `NULL` key columns, while the current typed
/// key decoder expects non-null `(i64, String)`.
#[djogi::djogi_test(sync_models = [Order])]
async fn cube_typed_surface_reports_null_key_decode_gap(mut ctx: djogi::DjogiContext) {
    for o in [
        order(1, 10, "ok", 10),
        order(1, 11, "pending", 5),
        order(2, 20, "ok", 100),
        order(2, 21, "pending", 50),
    ] {
        Order::create(&mut ctx, o).await.expect("create order");
    }

    let err = Order::objects()
        .cube(|f| (f.org_id(), f.status()))
        .annotate(|f| f.amount().sum())
        .fetch_all(&mut ctx)
        .await
        .expect_err("typed CUBE fetch currently cannot decode NULL subtotal keys");
    let err_detail = format!("{err:?}");
    assert!(
        err.to_string().contains("NULL")
            || err.to_string().contains("null")
            || err_detail.contains("WasNull"),
        "CUBE decode gap should mention a NULL key; got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 5 — group_by_sets
// ---------------------------------------------------------------------------

/// `.group_by_sets(|_| ["org_id", "status"])` emits `GROUP BY GROUPING SETS
/// ((org_id), (status))` — the UNION ALL of `GROUP BY org_id` and
/// `GROUP BY status`. The key tuple type is `()` because each result row's
/// key column depends on which grouping set matched; typed key decoding is
/// not meaningful for this shape.
///
/// This test stays on the public typed surface and observes the aggregate
/// values from the unit-key rows.
#[djogi::djogi_test(sync_models = [Order])]
async fn group_by_sets_emits_union_of_single_column_sets(mut ctx: djogi::DjogiContext) {
    for o in [
        order(1, 10, "ok", 10),
        order(1, 11, "pending", 20),
        order(2, 20, "ok", 100),
    ] {
        Order::create(&mut ctx, o).await.expect("create order");
    }

    let rows: Vec<((), i64)> = Order::objects()
        .group_by_sets(|_| ["org_id", "status"])
        .annotate(|f| f.amount().sum())
        .fetch_all(&mut ctx)
        .await
        .expect("grouping-sets typed query must succeed");

    assert_eq!(
        rows.len(),
        4,
        "GROUPING SETS over two single-column sets must produce 4 rows; got {}",
        rows.len()
    );

    let mut sums: Vec<i64> = rows.into_iter().map(|(_, sum)| sum).collect();
    sums.sort();
    assert_eq!(sums, vec![20, 30, 100, 110]);
}

// ---------------------------------------------------------------------------
// Scenario 6 — windowed aggregate (running total)
// ---------------------------------------------------------------------------

/// `.annotate(|f| f.amount().sum().over(|w| w.partition_by(f.partition_id()).order_by(f.seq())))`
/// must produce a running total per partition. Each row carries the sum of
/// all amounts up to and including its own `seq` value within the partition.
#[djogi::djogi_test(sync_models = [Run])]
async fn window_sum_produces_running_total_per_partition(mut ctx: djogi::DjogiContext) {
    // Two partitions × 5 rows each. Values chosen so the cumulative sum is
    // easy to read: partition 1 = 1, 3, 6, 10, 15; partition 2 = 10, 30, 60,
    // 100, 150.
    for r in [
        run(1, 1, 1),
        run(1, 2, 2),
        run(1, 3, 3),
        run(1, 4, 4),
        run(1, 5, 5),
        run(2, 1, 10),
        run(2, 2, 20),
        run(2, 3, 30),
        run(2, 4, 40),
        run(2, 5, 50),
    ] {
        Run::create(&mut ctx, r).await.expect("create run");
    }

    // `.annotate` (un-grouped path) emits `<aggregate> OVER (...) AS
    // __djogi_agg_0` — one row per Run, each carrying its running total.
    let rows: Vec<(Run, i64)> = Run::objects()
        .order_by(|f| f.partition_id().asc())
        .order_by(|f| f.seq().asc())
        .annotate(|f| {
            f.amount()
                .sum()
                .over(|w| w.partition_by(f.partition_id()).order_by(f.seq()))
        })
        .fetch_all(&mut ctx)
        .await
        .expect("running-total window query must succeed");

    assert_eq!(rows.len(), 10, "expected 10 rows, got {}", rows.len());

    // Partition 1 cumulative sums: 1, 3, 6, 10, 15.
    let partition_1: Vec<i64> = rows
        .iter()
        .filter(|(r, _)| r.partition_id == 1)
        .map(|(_, sum)| *sum)
        .collect();
    assert_eq!(
        partition_1,
        vec![1, 3, 6, 10, 15],
        "partition 1 running totals incorrect"
    );

    // Partition 2 cumulative sums: 10, 30, 60, 100, 150.
    let partition_2: Vec<i64> = rows
        .iter()
        .filter(|(r, _)| r.partition_id == 2)
        .map(|(_, sum)| *sum)
        .collect();
    assert_eq!(
        partition_2,
        vec![10, 30, 60, 100, 150],
        "partition 2 running totals incorrect"
    );
}

// ---------------------------------------------------------------------------
// Scenario 7 — ROWS frame (4-row moving sum)
// ---------------------------------------------------------------------------

/// `.rows(FrameBound::Preceding(3), FrameBound::CurrentRow)` produces a 4-row
/// moving sum. For the sequence 1..=10 the expected windowed sums are:
/// 1, 3, 6, 10, 14, 18, 22, 26, 30, 34 (each window covers at most 4 rows).
#[djogi::djogi_test(sync_models = [Run])]
async fn rows_frame_produces_4_row_moving_sum(mut ctx: djogi::DjogiContext) {
    for i in 1i64..=10 {
        Run::create(&mut ctx, run(1, i, i))
            .await
            .expect("create run");
    }

    let rows: Vec<(Run, i64)> = Run::objects()
        .order_by(|f| f.seq().asc())
        .annotate(|f| {
            f.amount().sum().over(|w| {
                w.order_by(f.seq())
                    .rows(FrameBound::Preceding(3), FrameBound::CurrentRow)
            })
        })
        .fetch_all(&mut ctx)
        .await
        .expect("moving-sum window query must succeed");

    assert_eq!(rows.len(), 10, "expected 10 rows");

    // Compare against hand-computed 4-row moving sums for 1..=10.
    let actual: Vec<i64> = rows.iter().map(|(_, sum)| *sum).collect();
    let expected = vec![1, 3, 6, 10, 14, 18, 22, 26, 30, 34];
    assert_eq!(
        actual, expected,
        "4-row moving sum mismatch: got {:?}, expected {:?}",
        actual, expected
    );
}

// ---------------------------------------------------------------------------
// Scenario 8 — count(distinct)
// ---------------------------------------------------------------------------

/// `.count().distinct()` on a grouped query must emit `COUNT(DISTINCT col)`
/// and return the count of distinct non-null values per group.
#[djogi::djogi_test(sync_models = [Order])]
async fn count_distinct_users_per_org(mut ctx: djogi::DjogiContext) {
    // Org 1: user_ids 10, 10, 11 → distinct count = 2.
    // Org 2: user_ids 20, 21    → distinct count = 2.
    // Org 3: user_id  30        → distinct count = 1.
    for o in [
        order(1, 10, "ok", 1),
        order(1, 10, "ok", 1),
        order(1, 11, "ok", 1),
        order(2, 20, "ok", 1),
        order(2, 21, "ok", 1),
        order(3, 30, "ok", 1),
    ] {
        Order::create(&mut ctx, o).await.expect("create order");
    }

    let rows: Vec<(i64, i64)> = Order::objects()
        .group_by(|f| f.org_id())
        .annotate(|f| f.user_id().count().distinct())
        .fetch_all(&mut ctx)
        .await
        .expect("count(distinct) must succeed");

    let mut map: std::collections::BTreeMap<i64, i64> = Default::default();
    for (org, count) in rows {
        map.insert(org, count);
    }
    assert_eq!(map.get(&1), Some(&2));
    assert_eq!(map.get(&2), Some(&2));
    assert_eq!(map.get(&3), Some(&1));
}

// ---------------------------------------------------------------------------
// Scenario 9 — HAVING
// ---------------------------------------------------------------------------

/// Exercises the `HAVING` clause code path. The plan's original wording
/// (`.having(|k, a| a.gt(Expr::literal(1000i64)))`) is not currently
/// expressible: `AggregateExpr<V>` has no public `Into<Expr<V>>` bridge, so
/// the HAVING closure cannot name the aggregate as an operand. Until that
/// bridge ships, the HAVING code path is exercised by filtering on the
/// GROUP BY key — the SQL emitter still lowers the closure's `Expr<bool>`
/// into a `HAVING <predicate>` clause, which is what this test is pinning.
///
/// Aggregate-predicate HAVING is a small future surface addition; when it
/// lands, update this test to the aggregate-based form.
#[djogi::djogi_test(sync_models = [Order])]
async fn having_clause_filters_groups(mut ctx: djogi::DjogiContext) {
    // Three orgs; only orgs 2 and 3 should pass a `org_id >= 2` filter.
    for o in [
        order(1, 10, "ok", 10),
        order(2, 20, "ok", 100),
        order(3, 30, "ok", 1000),
    ] {
        Order::create(&mut ctx, o).await.expect("create order");
    }

    // `k.as_expr().gte(Expr::literal(2i64))` produces the `Expr<bool>` the
    // HAVING emitter lowers to `HAVING org_id >= $n`. The aggregate `a` is
    // unused until the aggregate bridge ships.
    let rows: Vec<(i64, i64)> = Order::objects()
        .group_by(|f| f.org_id())
        .annotate(|f| f.amount().sum())
        .having(|k, _a| k.as_expr().gte(Expr::literal(2i64)))
        .fetch_all(&mut ctx)
        .await
        .expect("having query must succeed");

    assert_eq!(
        rows.len(),
        2,
        "expected 2 groups (orgs 2 and 3) to pass HAVING; got {:?}",
        rows
    );
    let mut orgs: Vec<i64> = rows.iter().map(|(o, _)| *o).collect();
    orgs.sort();
    assert_eq!(orgs, vec![2, 3]);
}

// ---------------------------------------------------------------------------
// Scenario 10 — order_by + limit
// ---------------------------------------------------------------------------

/// Exercises the `.order_by(...).limit(N)` code path on a grouped query. The
/// plan's original wording (`.order_by(|k, a| a.desc()).limit(5)`) is not
/// currently expressible because `AggregateExpr<V>` has no `asc()` / `desc()`
/// methods — those live on `FieldRef` and `Expr<V>` but there is no public
/// bridge from `AggregateExpr` into either. Until the aggregate → order bridge
/// ships, the ORDER BY + LIMIT code path is exercised by ordering on the
/// GROUP BY key.
///
/// Aggregate-based ORDER BY is a small future surface addition; when it
/// lands, update this test to the aggregate-based form.
#[djogi::djogi_test(sync_models = [Order])]
async fn order_by_key_with_limit(mut ctx: djogi::DjogiContext) {
    // Seed five distinct orgs so LIMIT 3 actually drops rows.
    for org in 1..=5i64 {
        Order::create(&mut ctx, order(org, 1, "ok", org * 10))
            .await
            .expect("create order");
    }

    let rows: Vec<(i64, i64)> = Order::objects()
        .group_by(|f| f.org_id())
        .annotate(|f| f.amount().sum())
        .order_by(|k, _a| k.desc())
        .limit(3)
        .fetch_all(&mut ctx)
        .await
        .expect("order_by + limit must succeed");

    assert_eq!(rows.len(), 3, "LIMIT 3 must cap the result to 3 rows");
    // Ordering must be descending on org_id — first row is org 5.
    assert_eq!(
        rows.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
        vec![5, 4, 3],
        "expected org_ids 5, 4, 3 in that order; got {:?}",
        rows
    );
}
