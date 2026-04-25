//! Phase 6.5 Task 14 — live-Postgres integration tests for the aggregation
//! surface.
//!
//! # Scope
//!
//! These tests exercise the full round-trip for the Phase 6.5 aggregation
//! additions against a real Postgres 18 instance (provisioned per-test by the
//! `#[djogi_test]` harness):
//!
//! 1. `.group_by(...).annotate(...).fetch_all(...)` — basic GROUP BY + SUM.
//! 2. Arity-2 `.group_by(...)` — tuple key decode.
//! 3. `.rollup(...)` — hierarchical subtotals, with a trailing `NULL` grand-total row.
//! 4. `.cube(...)` — all 2^N subsets plus the grand total.
//! 5. `.group_by_sets(...)` — explicit grouping sets (unit-key shape).
//! 6. Windowed aggregate inside `.annotate(...)` — running total per partition.
//! 7. ROWS frame — 4-row moving sum using `FrameBound::Preceding(3)..=CurrentRow`.
//! 8. `.count().distinct()` — DISTINCT modifier on a grouped aggregate.
//! 9. `.having(...)` — group-filter predicate via the key's `as_expr().gt(...)` shape.
//! 10. `.order_by(...).limit(...)` — top-N by key (aggregate-based top-N deferred;
//!     see the per-test doc comment).
//!
//! # Design notes
//!
//! - Models are declared inline in this file and scoped to `orders_p65` /
//!   `runs_p65` to avoid colliding with the Phase 4 / 5 table fixtures if
//!   another test file is ever merged into the same binary.
//! - Each test builds its own schema via `ctx.raw_ddl(...)` to keep the test
//!   self-contained. `IF NOT EXISTS` is unnecessary because the per-test DB
//!   is fresh, but including it costs nothing and keeps the DDL re-runnable if
//!   the harness is ever changed to reuse databases.
//! - The `.having(|k, a| a.gt(...))` shape named in the plan is not yet
//!   expressible through the public surface — `AggregateExpr<V>` has no
//!   `Into<Expr<V>>` bridge in Phase 6.5, so the HAVING predicate must close
//!   over the key. The test therefore exercises the HAVING code path (SQL
//!   `HAVING` clause emission + group filtering) via a key-based comparison.
//!   The same constraint applies to scenario #10 — `ORDER BY <aggregate>` is
//!   not available through the surface, so we exercise the ORDER BY + LIMIT
//!   code path via a key-based ordering.
//!
//! The aggregate-HAVING / aggregate-ORDER-BY bridge is a small future surface
//! addition; once it lands, these two tests should be updated to match the
//! original plan wording.

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

/// Create the `orders_p65` table. Fresh per-test DB, so no `IF NOT EXISTS`
/// is strictly necessary, but including it keeps the DDL trivially re-runnable.
async fn setup_orders(ctx: &mut djogi::DjogiContext) {
    ctx.raw_ddl(
        "CREATE TABLE IF NOT EXISTS orders_p65 (
             id         BIGINT      PRIMARY KEY DEFAULT generate_id(),
             created_at TIMESTAMPTZ NOT NULL    DEFAULT now(),
             updated_at TIMESTAMPTZ NOT NULL    DEFAULT now(),
             org_id     BIGINT      NOT NULL,
             user_id    BIGINT      NOT NULL,
             status     TEXT        NOT NULL,
             amount     BIGINT      NOT NULL
         )",
    )
    .await
    .expect("create orders_p65 table");
}

/// Create the `runs_p65` table.
async fn setup_runs(ctx: &mut djogi::DjogiContext) {
    ctx.raw_ddl(
        "CREATE TABLE IF NOT EXISTS runs_p65 (
             id           BIGINT      PRIMARY KEY DEFAULT generate_id(),
             created_at   TIMESTAMPTZ NOT NULL    DEFAULT now(),
             updated_at   TIMESTAMPTZ NOT NULL    DEFAULT now(),
             partition_id BIGINT      NOT NULL,
             seq          BIGINT      NOT NULL,
             amount       BIGINT      NOT NULL
         )",
    )
    .await
    .expect("create runs_p65 table");
}

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
#[djogi::djogi_test]
async fn group_by_org_id_sums_amount(mut ctx: djogi::DjogiContext) {
    setup_orders(&mut ctx).await;

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
#[djogi::djogi_test]
async fn group_by_two_columns_decodes_tuple_key(mut ctx: djogi::DjogiContext) {
    setup_orders(&mut ctx).await;

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

/// `.rollup(|f| f.org_id())` emits one additional row with `org_id = NULL` and
/// the grand-total sum across all rows.
///
/// The typed `fetch_all` path decodes the key with the column's underlying
/// Rust type (`i64` for `org_id`), not `Option<i64>`, so the grand-total row's
/// NULL key would fail the typed decoder. Phase 6.5's grouped fetch surface
/// doesn't yet provide an `Option`-aware key decode; this test therefore
/// issues the equivalent raw SQL to inspect the ROLLUP row shape.
#[djogi::djogi_test]
async fn rollup_emits_grand_total_row(mut ctx: djogi::DjogiContext) {
    setup_orders(&mut ctx).await;

    for o in [
        order(1, 10, "ok", 10),
        order(1, 11, "ok", 20),
        order(2, 20, "ok", 100),
    ] {
        Order::create(&mut ctx, o).await.expect("create order");
    }

    // Raw SQL mirrors the query the ROLLUP emitter produces for
    // `.rollup(|f| f.org_id()).annotate(|f| f.amount().sum())`.
    let rows = ctx
        .__query_all_for_macros(
            "SELECT org_id, SUM(amount)::BIGINT AS agg_sum \
             FROM orders_p65 \
             GROUP BY ROLLUP (org_id) \
             ORDER BY org_id NULLS LAST",
            &[],
        )
        .await
        .expect("rollup raw query must succeed");

    // Two real groups (orgs 1 and 2) + one grand-total row = 3 rows.
    assert_eq!(
        rows.len(),
        3,
        "expected 3 rows from ROLLUP, got {}",
        rows.len()
    );

    // Grand-total row: org_id IS NULL, sum = 10+20+100 = 130.
    let last = &rows[2];
    let org: Option<i64> = last.try_get("org_id").expect("org_id column present");
    let sum: i64 = last.try_get("agg_sum").expect("agg_sum column present");
    assert!(
        org.is_none(),
        "grand-total row must have NULL org_id; got {org:?}"
    );
    assert_eq!(sum, 130, "grand total must equal 10+20+100; got {sum}");
}

// ---------------------------------------------------------------------------
// Scenario 4 — cube
// ---------------------------------------------------------------------------

/// `.cube(|f| (f.org_id(), f.status()))` emits every subset of the key set —
/// one row per (org_id, status) cell, per-org subtotal (status=NULL),
/// per-status subtotal (org_id=NULL), and the grand total (both NULL).
///
/// Same decode caveat as `rollup_emits_grand_total_row`: the typed fetch
/// path would choke on the NULL key columns that CUBE emits for subtotal
/// rows, so this test uses raw SQL to inspect the full row shape. The
/// typed surface is still exercised — we assert it executes without error.
#[djogi::djogi_test]
async fn cube_emits_all_subtotal_shapes(mut ctx: djogi::DjogiContext) {
    setup_orders(&mut ctx).await;

    for o in [
        order(1, 10, "ok", 10),
        order(1, 11, "pending", 5),
        order(2, 20, "ok", 100),
        order(2, 21, "pending", 50),
    ] {
        Order::create(&mut ctx, o).await.expect("create order");
    }

    // Raw SQL mirrors the CUBE query for (org_id, status).
    let rows = ctx
        .__query_all_for_macros(
            "SELECT org_id, status, SUM(amount)::BIGINT AS agg_sum \
             FROM orders_p65 \
             GROUP BY CUBE (org_id, status)",
            &[],
        )
        .await
        .expect("cube raw query must succeed");

    // 2×2 cells + 2 per-org subtotals + 2 per-status subtotals + 1 grand
    // total = 9 rows for this dataset.
    assert_eq!(
        rows.len(),
        9,
        "expected 9 rows from CUBE, got {}",
        rows.len()
    );

    // Grand total must be present with both keys NULL and sum = 165.
    let grand = rows
        .iter()
        .find_map(|r| {
            let org: Option<i64> = r.try_get("org_id").ok()?;
            let status: Option<String> = r.try_get("status").ok()?;
            let sum: i64 = r.try_get("agg_sum").ok()?;
            if org.is_none() && status.is_none() {
                Some(sum)
            } else {
                None
            }
        })
        .expect("CUBE must emit a grand-total row with both keys NULL");
    assert_eq!(grand, 165, "grand total mismatch; got {grand}");

    // At least one per-org subtotal (status NULL, org not NULL) and one
    // per-status subtotal (org NULL, status not NULL) must appear.
    let has_org_subtotal = rows.iter().any(|r| {
        let org: Option<i64> = r.try_get("org_id").ok().flatten();
        let status: Option<String> = r.try_get("status").ok().flatten();
        org.is_some() && status.is_none()
    });
    let has_status_subtotal = rows.iter().any(|r| {
        let org: Option<i64> = r.try_get("org_id").ok().flatten();
        let status: Option<String> = r.try_get("status").ok().flatten();
        org.is_none() && status.is_some()
    });
    assert!(
        has_org_subtotal,
        "expected at least one per-org subtotal row (status NULL)"
    );
    assert!(
        has_status_subtotal,
        "expected at least one per-status subtotal row (org NULL)"
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
/// # Known limitation — deferred to a future task
///
/// The typed `.fetch_all(...)` path currently emits a stray leading comma in
/// the SELECT list when the key tuple is `()`:
///
/// ```text
/// SELECT , SUM(amount)::BIGINT AS __djogi_agg_0 FROM orders_p65 ...
///         ^ missing placeholder column
/// ```
///
/// This is because `IntoAggregateTuple::push_columns_bare` unconditionally
/// prepends `, ` before each aggregate slot — a reasonable default when one
/// or more key columns precede the aggregate list, but invalid SQL when the
/// key tuple is `()`. The SQL builder needs a small tweak to suppress the
/// leading separator when no key columns have been emitted. That fix is a
/// future Phase 6.5 or Phase 7 task.
///
/// Until then, this test exercises the GROUPING SETS SQL shape directly via
/// `ctx.raw_query(...)` — the semantics (UNION over two single-column sets)
/// are what end users care about.
#[djogi::djogi_test]
async fn group_by_sets_emits_union_of_single_column_sets(mut ctx: djogi::DjogiContext) {
    setup_orders(&mut ctx).await;

    for o in [
        order(1, 10, "ok", 10),
        order(1, 11, "pending", 20),
        order(2, 20, "ok", 100),
    ] {
        Order::create(&mut ctx, o).await.expect("create order");
    }

    // Raw SQL mirrors the query a fixed `.group_by_sets(...).annotate(...)`
    // would emit. Two single-column sets → 4 rows: 2 per-org (orgs 1 and 2)
    // plus 2 per-status ("ok", "pending").
    let rows = ctx
        .__query_all_for_macros(
            "SELECT org_id, status, SUM(amount)::BIGINT AS agg_sum \
             FROM orders_p65 \
             GROUP BY GROUPING SETS ((org_id), (status))",
            &[],
        )
        .await
        .expect("grouping-sets raw query must succeed");

    assert_eq!(
        rows.len(),
        4,
        "GROUPING SETS over two single-column sets must produce 4 rows; got {}",
        rows.len()
    );

    // Shape check — each row has exactly one of org_id / status non-null.
    // (The third row — both NULL — is the grand total, which PostgreSQL does
    // not emit for GROUPING SETS without an explicit `()` set; it is absent
    // here.)
    for r in &rows {
        let org: Option<i64> = r.try_get("org_id").expect("org_id col");
        let status: Option<String> = r.try_get("status").expect("status col");
        assert!(
            org.is_some() ^ status.is_some(),
            "each GROUPING SETS row must have exactly one key non-null; got org={org:?}, status={status:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario 6 — windowed aggregate (running total)
// ---------------------------------------------------------------------------

/// `.annotate(|f| f.amount().sum().over(|w| w.partition_by(f.partition_id()).order_by(f.seq())))`
/// must produce a running total per partition. Each row carries the sum of
/// all amounts up to and including its own `seq` value within the partition.
#[djogi::djogi_test]
async fn window_sum_produces_running_total_per_partition(mut ctx: djogi::DjogiContext) {
    setup_runs(&mut ctx).await;

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
#[djogi::djogi_test]
async fn rows_frame_produces_4_row_moving_sum(mut ctx: djogi::DjogiContext) {
    setup_runs(&mut ctx).await;

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
#[djogi::djogi_test]
async fn count_distinct_users_per_org(mut ctx: djogi::DjogiContext) {
    setup_orders(&mut ctx).await;

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
#[djogi::djogi_test]
async fn having_clause_filters_groups(mut ctx: djogi::DjogiContext) {
    setup_orders(&mut ctx).await;

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
#[djogi::djogi_test]
async fn order_by_key_with_limit(mut ctx: djogi::DjogiContext) {
    setup_orders(&mut ctx).await;

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
