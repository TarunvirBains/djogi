//! Phase 2 QuerySet integration tests.

use djogi::prelude::*;

// Separate table name (`posts_p2`) so this integration test can share a DB
// with `phase1_model.rs` without DDL collisions.
// Phase 7-Zero-2 T2 default flip — pin ascending HeerId so existing
// HeerId-typed construction and assertions keep working.
#[model(table = "posts_p2", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Post {
    pub title: String,
    pub body: String,
    pub published: bool,
    pub view_count: i32,
    /// Nullable companion column — lets integration tests exercise
    /// `IS NULL` / `NULLS FIRST` / `NULLS LAST` ordering semantics
    /// without fighting `view_count`'s `NOT NULL` constraint. Left
    /// `NULL` for the four `seed_posts` rows; individual tests backfill
    /// it (or seed an additional NULL-bearing row) as needed.
    pub score: Option<i32>,
}

/// Install HeeRanjId schema + seed node 1 + create `posts_p2`. HeeRanjID
/// schema, node seeding, and `heer.node_id` database-level setting are all
/// handled by `#[djogi_test]`'s bootstrap — this only creates the table.
async fn setup(ctx: &mut djogi::DjogiContext) {
    ctx.raw_execute(
        "CREATE TABLE posts_p2 (
            id          BIGINT      PRIMARY KEY DEFAULT generate_id(),
            created_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            updated_at  TIMESTAMPTZ NOT NULL    DEFAULT now(),
            title       TEXT        NOT NULL,
            body        TEXT        NOT NULL,
            published   BOOLEAN     NOT NULL,
            view_count  INTEGER     NOT NULL,
            score       INTEGER
        )",
        &[],
    )
    .await
    .unwrap();
}

// ── Task 5: lazy builder compile surface ──────────────────────────────────

#[djogi::djogi_test]
async fn objects_returns_empty_queryset(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;

    // `T::objects()` resolves and returns a lazy builder. No SQL has been
    // emitted or executed — Task 5 deliberately ships without terminal
    // methods.
    let qs = Post::objects();

    // Builder methods compose without executing. The clone proves
    // `QuerySet: Clone` (important for `if/else` branches that reuse a
    // partially-built queryset).
    let _qs2 = qs.clone().filter(|f| f.published().eq(true)).limit(10);

    // `exclude` + chained `order_by` + `offset` — covers the rest of the
    // Task 5 builder surface at compile time.
    let _qs3 = qs
        .clone()
        .exclude(|f| f.title().eq("draft".to_string()))
        .order_by(|f| f.view_count().desc())
        .order_by(|f| f.title().asc())
        .offset(5);

    // `none()` short-circuit branch + `distinct`/`distinct_on` — closes out
    // every public QuerySet method the task introduces. `none()` is an
    // instance method (matching Django's `queryset.none()` ergonomics) so
    // both `Post::objects().none()` and from-scratch construction via
    // `QuerySet::<Post>::new().none()` compile.
    let _empty_from_objects: QuerySet<Post> = Post::objects().none();
    let _empty_from_scratch: QuerySet<Post> = QuerySet::<Post>::new().none();
    let _distinct = Post::objects().distinct();
    let _distinct_on = Post::objects().distinct_on(|f| f.title());
}

// ── Task 6: terminal read methods ─────────────────────────────────────────
//
// Each test seeds four deterministic rows via `seed_posts` and then
// exercises exactly one terminal on a filter/composition that surfaces a
// distinct branch of the SQL emitter or terminal-method contract.

/// Seed four deterministic posts. Uses `raw_execute` inside a transaction
/// so `generate_id()` works on the same connection that already has
/// heer.node_id set (inherits from ALTER DATABASE done by djogi_test bootstrap).
async fn seed_posts(ctx: &mut djogi::DjogiContext) {
    let mut tx = ctx.begin().await.unwrap();
    // `score` is a nullable companion column used by the NULLS-ordering
    // test; seeding it with a distinct value per row lets that test assert
    // a deterministic sort order once a fifth NULL-bearing row is added.
    // Other tests do not read `score` so its presence is invisible to them.
    for (title, published, views, score) in [
        ("alpha", true, 100i32, 10i32),
        ("beta", true, 50, 20),
        ("gamma", false, 200, 30),
        ("delta", true, 25, 40),
    ] {
        let title_s = title.to_string();
        let body_s = "body".to_string();
        tx.raw_execute(
            "INSERT INTO posts_p2 (title, body, published, view_count, score) \
             VALUES ($1, $2, $3, $4, $5)",
            &[&title_s, &body_s, &published, &views, &score],
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
}

#[djogi::djogi_test]
async fn fetch_all_no_filter(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;
    let rows = Post::objects().fetch_all(&mut ctx).await.unwrap();
    assert_eq!(rows.len(), 4);
}

#[djogi::djogi_test]
async fn fetch_all_with_filter(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;
    let rows = Post::objects()
        .filter(|f| f.published().eq(true))
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
}

#[djogi::djogi_test]
async fn fetch_one_exact_match(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;
    let row = Post::objects()
        .filter(|f| f.title().eq("alpha".to_string()))
        .fetch_one(&mut ctx)
        .await
        .unwrap();
    assert_eq!(row.title, "alpha");
}

#[djogi::djogi_test]
async fn fetch_one_zero_rows_is_not_found(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;
    let err = Post::objects()
        .filter(|f| f.title().eq("nonexistent".to_string()))
        .fetch_one(&mut ctx)
        .await
        .unwrap_err();
    assert!(matches!(err, DjogiError::NotFound { .. }));
}

#[djogi::djogi_test]
async fn fetch_one_multiple_rows_is_multiple_objects(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;
    let err = Post::objects()
        .filter(|f| f.published().eq(true))
        .fetch_one(&mut ctx)
        .await
        .unwrap_err();
    assert!(matches!(err, DjogiError::MultipleObjects { .. }));
}

#[djogi::djogi_test]
async fn first_returns_some_or_none(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;
    let some = Post::objects()
        .filter(|f| f.published().eq(true))
        .first(&mut ctx)
        .await
        .unwrap();
    assert!(some.is_some());

    let none = Post::objects()
        .filter(|f| f.title().eq("nope".to_string()))
        .first(&mut ctx)
        .await
        .unwrap();
    assert!(none.is_none());
}

#[djogi::djogi_test]
async fn count_returns_row_count(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;
    let n = Post::objects().count(&mut ctx).await.unwrap();
    assert_eq!(n, 4);
    let n2 = Post::objects()
        .filter(|f| f.published().eq(true))
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n2, 3);
}

#[djogi::djogi_test]
async fn exists_returns_bool(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;
    assert!(
        Post::objects()
            .filter(|f| f.title().eq("alpha".to_string()))
            .exists(&mut ctx)
            .await
            .unwrap()
    );
    assert!(
        !Post::objects()
            .filter(|f| f.title().eq("nope".to_string()))
            .exists(&mut ctx)
            .await
            .unwrap()
    );
}

#[djogi::djogi_test]
async fn none_short_circuits_every_terminal(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;

    // `fetch_all` -> Ok(vec![])
    let empty = Post::objects().none().fetch_all(&mut ctx).await.unwrap();
    assert!(empty.is_empty());

    // `count` -> Ok(0)
    assert_eq!(Post::objects().none().count(&mut ctx).await.unwrap(), 0);

    // `exists` -> Ok(false)
    assert!(!Post::objects().none().exists(&mut ctx).await.unwrap());

    // `first` -> Ok(None)
    assert!(
        Post::objects()
            .none()
            .first(&mut ctx)
            .await
            .unwrap()
            .is_none()
    );

    // `fetch_one` -> Err(NotFound)
    let none_err = Post::objects()
        .none()
        .fetch_one(&mut ctx)
        .await
        .unwrap_err();
    assert!(matches!(none_err, DjogiError::NotFound { .. }));
}

#[djogi::djogi_test]
async fn limit_offset_paginate(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;
    let page1 = Post::objects()
        .order_by(|f| f.title().asc())
        .limit(2)
        .offset(0)
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    let page2 = Post::objects()
        .order_by(|f| f.title().asc())
        .limit(2)
        .offset(2)
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(page1.len(), 2);
    assert_eq!(page2.len(), 2);
    assert_ne!(page1[0].title, page2[0].title);
}

#[djogi::djogi_test]
async fn nested_and_or(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;
    let rows = Post::objects()
        .filter(|f| f.published().eq(true).and_with(f.view_count().gte(50i32)))
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    // alpha (views=100, published) + beta (views=50, published) match; delta
    // (views=25) and gamma (unpublished) do not.
    assert_eq!(rows.len(), 2);
}

#[djogi::djogi_test]
async fn in_list_and_between(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;

    let by_title = Post::objects()
        .filter(|f| {
            f.title()
                .in_list(vec!["alpha".to_string(), "beta".to_string()])
        })
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(by_title.len(), 2);

    let by_views = Post::objects()
        .filter(|f| f.view_count().between(40i32, 120i32))
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    // alpha (100) + beta (50) fall inside [40, 120]; delta (25) and gamma
    // (200) do not.
    assert_eq!(by_views.len(), 2);
}

#[djogi::djogi_test]
async fn filter_struct_matches_closure_results(mut ctx: djogi::DjogiContext) {
    // Task 8 parity check: `filter_struct` (programmatic) and `filter`
    // (closure) must produce structurally equivalent filters for the
    // same set of lookups.
    use std::collections::BTreeSet;

    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;

    let closure_rows = Post::objects()
        .filter(|f| f.published().eq(true).and_with(f.view_count().gte(50i32)))
        .fetch_all(&mut ctx)
        .await
        .unwrap();

    let filter = PostFilter::new()
        .published(Lookup::Eq(true))
        .view_count(Lookup::Gte(50i32));
    let struct_rows = Post::objects()
        .filter_struct(filter)
        .fetch_all(&mut ctx)
        .await
        .unwrap();

    assert_eq!(
        closure_rows.len(),
        struct_rows.len(),
        "closure filter and struct filter must return the same row count"
    );

    let closure_ids: BTreeSet<_> = closure_rows.iter().map(|p| p.id).collect();
    let struct_ids: BTreeSet<_> = struct_rows.iter().map(|p| p.id).collect();
    assert_eq!(
        closure_ids, struct_ids,
        "closure filter and struct filter must return the same row set"
    );

    assert_eq!(struct_rows.len(), 2);
}

#[djogi::djogi_test]
async fn filter_struct_empty_is_identity(mut ctx: djogi::DjogiContext) {
    // A filter with zero setters should not AND anything onto the
    // queryset — terminal fetch should see every row `seed_posts`
    // inserted.
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;

    let empty = PostFilter::new();
    let rows = Post::objects()
        .filter_struct(empty)
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(rows.len(), 4);
}

#[djogi::djogi_test]
async fn filter_struct_single_clause_unwraps_to_leaf(mut ctx: djogi::DjogiContext) {
    // A single-clause filter should emit SQL equivalent to a bare leaf.
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;

    let closure_rows = Post::objects()
        .filter(|f| f.published().eq(true))
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    let struct_rows = Post::objects()
        .filter_struct(PostFilter::new().published(Lookup::Eq(true)))
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(closure_rows.len(), struct_rows.len());
    assert_eq!(struct_rows.len(), 3);
}

// ── Task 9: bulk update / delete ──────────────────────────────────────────

#[djogi::djogi_test]
async fn bulk_update_sets_values_and_returns_count(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;

    let n = Post::objects()
        .filter(|f| f.published().eq(true))
        .update(|f| f.view_count().set(999i32))
        .execute(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 3, "expected 3 published rows bumped");

    let bumped = Post::objects()
        .filter(|f| f.view_count().eq(999i32))
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(bumped, 3);
}

#[djogi::djogi_test]
async fn bulk_update_none_short_circuits(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;

    let n = Post::objects()
        .none()
        .update(|f| f.view_count().set(0i32))
        .execute(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 0);

    let unchanged = Post::objects().count(&mut ctx).await.unwrap();
    assert_eq!(unchanged, 4);

    let zeroed = Post::objects()
        .filter(|f| f.view_count().eq(0i32))
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(zeroed, 0, "none().update() must not touch any row");
}

#[djogi::djogi_test]
async fn bulk_delete_removes_rows_and_returns_count(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;

    let n = Post::objects()
        .filter(|f| f.published().eq(false))
        .delete(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 1);

    let remaining = Post::objects().count(&mut ctx).await.unwrap();
    assert_eq!(remaining, 3);

    let gamma_left = Post::objects()
        .filter(|f| f.title().eq("gamma".to_string()))
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(gamma_left, 0);
}

#[djogi::djogi_test]
async fn bulk_update_stamps_updated_at(mut ctx: djogi::DjogiContext) {
    // Contract: bulk update must always stamp `updated_at = now()`, even
    // when the user did not set it themselves.
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;

    // Every freshly-inserted row has updated_at = created_at (both default
    // to `now()` in the same INSERT).
    let all_equal: i64 = ctx
        .raw_scalar(
            "SELECT COUNT(*) FROM posts_p2 WHERE updated_at = created_at",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(all_equal, 4);

    // Sleep a tick so `now()` advances past the insert time at the
    // microsecond level. Postgres's `now()` is statement-start time; a
    // single millisecond is enough to push the new value past `created_at`.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let n = Post::objects()
        .filter(|f| f.title().eq("alpha".to_string()))
        .update(|f| f.view_count().set(42i32))
        .execute(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 1);

    // The bumped row's `updated_at` now exceeds its `created_at`.
    let bumped: i64 = ctx
        .raw_scalar(
            "SELECT COUNT(*) FROM posts_p2 WHERE title = 'alpha' AND updated_at > created_at",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        bumped, 1,
        "bulk update must stamp updated_at = now() on touched rows"
    );

    // Unaffected rows still have updated_at = created_at.
    let untouched: i64 = ctx
        .raw_scalar(
            "SELECT COUNT(*) FROM posts_p2 WHERE title <> 'alpha' AND updated_at = created_at",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(untouched, 3);
}

#[djogi::djogi_test]
async fn distinct_on_and_plain(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;
    // Add duplicate titles so DISTINCT ON has real work to do. Use
    // raw_execute inside a begin/commit block.
    let mut tx = ctx.begin().await.unwrap();
    tx.raw_execute(
        "INSERT INTO posts_p2 (title, body, published, view_count) \
         VALUES ('dup', 'x', true, 1), ('dup', 'y', true, 2)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let rows = Post::objects()
        .distinct_on(|f| f.title())
        .order_by(|f| f.title().asc())
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    // DISTINCT ON (title) keeps exactly one row per distinct title —
    // 'dup' collapses from 2 rows to 1.
    assert_eq!(
        rows.iter().filter(|p| p.title == "dup").count(),
        1,
        "distinct_on(title) should collapse duplicate titles"
    );

    let plain_distinct_count = Post::objects().distinct().count(&mut ctx).await.unwrap();
    let base_count = Post::objects().count(&mut ctx).await.unwrap();
    assert_eq!(
        plain_distinct_count, base_count,
        "PK makes every row unique — distinct count == base count"
    );
    let plain_rows = Post::objects()
        .distinct()
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(plain_rows.len() as i64, base_count);

    let distinct_on_count = Post::objects()
        .distinct_on(|f| f.title())
        .count(&mut ctx)
        .await
        .unwrap();
    assert!(
        distinct_on_count < base_count,
        "distinct_on(title) count ({distinct_on_count}) should be \
         less than base count ({base_count}) since 'dup' collapses"
    );
}

// ── Task 10: edge-case sweep ──────────────────────────────────────────────

/// `in_list(vec![])` must match zero rows.
#[djogi::djogi_test]
async fn in_list_empty_returns_zero_rows(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;

    let rows = Post::objects()
        .filter(|f| f.id().in_list(Vec::<HeerId>::new()))
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(rows.len(), 0, "empty IN list must match zero rows");

    let n = Post::objects()
        .filter(|f| f.id().in_list(Vec::<HeerId>::new()))
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 0, "empty IN list count must be 0");
}

/// `not_in_list(vec![])` must match every row.
#[djogi::djogi_test]
async fn not_in_list_empty_returns_all_rows(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;

    let n = Post::objects()
        .filter(|f| f.id().not_in_list(Vec::<HeerId>::new()))
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 4, "empty NOT IN list must match every row");
}

/// `contains(...)` must escape LIKE wildcards (`%`, `_`, `\`) in user input.
#[djogi::djogi_test]
async fn string_contains_escapes_percent_and_underscore(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;

    // Add the target row (contains both wildcard characters verbatim) plus
    // two negative-control rows that a broken escape would falsely match.
    let mut tx = ctx.begin().await.unwrap();
    tx.raw_execute(
        "INSERT INTO posts_p2 (title, body, published, view_count) VALUES \
         ('50% off_deal', 'b', true, 1), \
         ('50 off regular', 'b', true, 1), \
         ('xdeal', 'b', true, 1)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let total = Post::objects().count(&mut ctx).await.unwrap();
    assert_eq!(total, 7, "4 seeded + 3 extras must all be present");

    let pct = Post::objects()
        .filter(|f| f.title().contains("50%"))
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(pct.len(), 1, "contains('50%') must escape % literally");
    assert_eq!(pct[0].title, "50% off_deal");

    let und = Post::objects()
        .filter(|f| f.title().contains("_deal"))
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(und.len(), 1, "contains('_deal') must escape _ literally");
    assert_eq!(und[0].title, "50% off_deal");
}

/// `.exclude(|f| ...)` must wrap the inner filter in SQL `NOT`.
#[djogi::djogi_test]
async fn exclude_wraps_in_not(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;

    let n = Post::objects()
        .exclude(|f| f.published().eq(true))
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(
        n, 1,
        "exclude(published=true) keeps only the unpublished row"
    );
}

/// Successive `.order_by(...)` calls **stack** (Django semantics), not last-wins.
#[djogi::djogi_test]
async fn order_by_stacks_across_multiple_calls(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;

    let rows = Post::objects()
        .order_by(|f| f.published().desc())
        .order_by(|f| f.view_count().asc())
        .fetch_all(&mut ctx)
        .await
        .unwrap();

    let titles: Vec<&str> = rows.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["delta", "beta", "alpha", "gamma"],
        "multi-call order_by must stack (published DESC, view_count ASC), \
         not replace"
    );
}

/// `nulls_first()` / `nulls_last()` must render the corresponding SQL modifier.
#[djogi::djogi_test]
async fn order_by_nulls_first_renders(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;

    // Insert a fifth row with `score = NULL`.
    let mut tx = ctx.begin().await.unwrap();
    tx.raw_execute(
        "INSERT INTO posts_p2 (title, body, published, view_count, score) \
         VALUES ('nullrow', 'b', true, 0, NULL)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // `NULLS FIRST` — the NULL-score row floats to the top of an ASC ordering.
    let first_rows = Post::objects()
        .order_by(|f| f.score().asc().nulls_first())
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(
        first_rows.len(),
        5,
        "NULL row must be included — NULLS FIRST is pointless otherwise"
    );
    assert_eq!(
        first_rows[0].title, "nullrow",
        "NULLS FIRST must put the NULL-score row at position 0"
    );
    assert!(
        first_rows[0].score.is_none(),
        "first row's score must be NULL — confirms we're sorting by the right column"
    );

    // `NULLS LAST` — the NULL-score row sinks to the bottom of an ASC ordering.
    let last_rows = Post::objects()
        .order_by(|f| f.score().asc().nulls_last())
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(last_rows.len(), 5);
    assert_eq!(
        last_rows.last().unwrap().title,
        "nullrow",
        "NULLS LAST must put the NULL-score row at the tail"
    );
    assert!(
        last_rows.last().unwrap().score.is_none(),
        "last row's score must be NULL"
    );
}

/// `filter(...).update(|_| vec![])` must short-circuit to `Ok(0)`.
#[djogi::djogi_test]
async fn bulk_update_empty_assignments_short_circuits(mut ctx: djogi::DjogiContext) {
    setup(&mut ctx).await;
    seed_posts(&mut ctx).await;

    let n = Post::objects()
        .filter(|f| f.published().eq(true))
        .update(|_| Vec::<djogi::UpdateAssignment>::new())
        .execute(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 0, "empty assignment list must short-circuit to Ok(0)");

    let total = Post::objects().count(&mut ctx).await.unwrap();
    assert_eq!(total, 4, "empty-assignments update must not touch any row");

    let rows = Post::objects()
        .order_by(|f| f.view_count().asc())
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    let views: Vec<i32> = rows.iter().map(|p| p.view_count).collect();
    assert_eq!(
        views,
        vec![25, 50, 100, 200],
        "seeded view_count values must survive a short-circuited update"
    );

    let unstamped: i64 = ctx
        .raw_scalar(
            "SELECT COUNT(*) FROM posts_p2 WHERE updated_at = created_at",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        unstamped, 4,
        "updated_at must still equal created_at — no UPDATE fired"
    );
}
