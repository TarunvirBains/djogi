// Phase 2 QuerySet integration tests.

use djogi::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};

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

#[model(table = "phase2_returning_pair_long_aliases", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct ReturningPairLongAliasBulk {
    // 50 chars: below Postgres boundary when prefixed.
    pub xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx: i32,
    // 52 chars: boundary +1 when prefixed with "__djogi_old." / "__djogi_new.".
    pub xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxa: i32,
    // 52 chars, same first 51 chars as previous field to model truncation
    // collision pressure in legacy alias projection styles.
    pub xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxb: i32,
}

#[model(table = "phase2_bulk_outbox_evt_row", pk = HeerId, events)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct BulkOutboxEvtRow {
    pub tag: String,
    pub score: i32,
}

#[model(table = "phase2_bulk_outbox_no_evt_row", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct BulkOutboxNoEvtRow {
    pub score: i32,
}

#[model(table = "phase2_bulk_hooks_row", pk = HeerId, hooks)]
#[derive(Debug, Clone)]
pub struct BulkHooksRow {
    pub score: i32,
}

static BULK_BEFORE_SAVE_CALLS: AtomicUsize = AtomicUsize::new(0);
static BULK_AFTER_SAVE_CALLS: AtomicUsize = AtomicUsize::new(0);

impl djogi::hooks::ModelHooks for BulkHooksRow {
    async fn before_save(
        &mut self,
        _ctx: &mut djogi::DjogiContext,
    ) -> Result<(), djogi::DjogiError> {
        BULK_BEFORE_SAVE_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn after_save(&self, _ctx: &mut djogi::DjogiContext) -> Result<(), djogi::DjogiError> {
        BULK_AFTER_SAVE_CALLS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

async fn seed_returning_pair_long_alias_bulk_rows(
    ctx: &mut djogi::DjogiContext,
) -> Result<Vec<ReturningPairLongAliasBulk>, DjogiError> {
    let mut rows = Vec::new();
    for (a, b, c) in [(1, 11, 21), (2, 12, 22), (3, 13, 23)] {
        rows.push(
            ReturningPairLongAliasBulk::create(
                ctx,
                ReturningPairLongAliasBulk {
                    xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx: a,
                    xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxa: b,
                    xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxb: c,
                    ..Default::default()
                },
            )
            .await?,
        );
    }
    Ok(rows)
}

async fn seed_bulk_outbox_evt_rows(
    ctx: &mut djogi::DjogiContext,
) -> Result<Vec<BulkOutboxEvtRow>, DjogiError> {
    let mut rows = Vec::new();
    for i in 0..3 {
        rows.push(
            BulkOutboxEvtRow::create(
                ctx,
                BulkOutboxEvtRow {
                    tag: format!("evt-{i}"),
                    score: 10 + i,
                    ..Default::default()
                },
            )
            .await?,
        );
    }
    Ok(rows)
}

async fn seed_bulk_outbox_no_evt_rows(
    ctx: &mut djogi::DjogiContext,
) -> Result<Vec<BulkOutboxNoEvtRow>, DjogiError> {
    let mut rows = Vec::new();
    for i in 0..2 {
        rows.push(
            BulkOutboxNoEvtRow::create(
                ctx,
                BulkOutboxNoEvtRow {
                    score: 10 + i,
                    ..Default::default()
                },
            )
            .await?,
        );
    }
    Ok(rows)
}

async fn seed_bulk_hooks_rows(
    ctx: &mut djogi::DjogiContext,
) -> Result<Vec<BulkHooksRow>, DjogiError> {
    let mut rows = Vec::new();
    for i in 0..3 {
        rows.push(
            BulkHooksRow::create(
                ctx,
                BulkHooksRow {
                    score: 100 + i,
                    ..Default::default()
                },
            )
            .await?,
        );
    }
    Ok(rows)
}

// ── Task 5: lazy builder compile surface ──────────────────────────────────

#[djogi::djogi_test(sync_models = [Post])]
async fn objects_returns_empty_queryset(mut ctx: djogi::DjogiContext) {
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

async fn seed_posts(ctx: &mut djogi::DjogiContext) {
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
        create_post(ctx, title, "body", published, views, Some(score))
            .await
            .unwrap();
    }
}

async fn create_post(
    ctx: &mut djogi::DjogiContext,
    title: &str,
    body: &str,
    published: bool,
    view_count: i32,
    score: Option<i32>,
) -> Result<Post, DjogiError> {
    Post::create(
        ctx,
        Post {
            title: title.to_string(),
            body: body.to_string(),
            published,
            view_count,
            score,
            ..Default::default()
        },
    )
    .await
}

#[djogi::djogi_test(sync_models = [Post])]
async fn fetch_all_no_filter(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;
    let rows = Post::objects().fetch_all(&mut ctx).await.unwrap();
    assert_eq!(rows.len(), 4);
}

#[djogi::djogi_test(sync_models = [Post])]
async fn fetch_all_with_filter(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;
    let rows = Post::objects()
        .filter(|f| f.published().eq(true))
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
}

#[djogi::djogi_test(sync_models = [Post])]
async fn fetch_one_exact_match(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;
    let row = Post::objects()
        .filter(|f| f.title().eq("alpha".to_string()))
        .fetch_one(&mut ctx)
        .await
        .unwrap();
    assert_eq!(row.title, "alpha");
}

#[djogi::djogi_test(sync_models = [Post])]
async fn fetch_one_zero_rows_is_not_found(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;
    let err = Post::objects()
        .filter(|f| f.title().eq("nonexistent".to_string()))
        .fetch_one(&mut ctx)
        .await
        .unwrap_err();
    assert!(matches!(err, DjogiError::NotFound { .. }));
}

#[djogi::djogi_test(sync_models = [Post])]
async fn fetch_one_multiple_rows_is_multiple_objects(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;
    let err = Post::objects()
        .filter(|f| f.published().eq(true))
        .fetch_one(&mut ctx)
        .await
        .unwrap_err();
    assert!(matches!(err, DjogiError::MultipleObjects { .. }));
}

#[djogi::djogi_test(sync_models = [Post])]
async fn first_returns_some_or_none(mut ctx: djogi::DjogiContext) {
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

#[djogi::djogi_test(sync_models = [Post])]
async fn count_returns_row_count(mut ctx: djogi::DjogiContext) {
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

#[djogi::djogi_test(sync_models = [Post])]
async fn exists_returns_bool(mut ctx: djogi::DjogiContext) {
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

#[djogi::djogi_test(sync_models = [Post])]
async fn none_short_circuits_every_terminal(mut ctx: djogi::DjogiContext) {
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

#[djogi::djogi_test(sync_models = [Post])]
async fn limit_offset_paginate(mut ctx: djogi::DjogiContext) {
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

#[djogi::djogi_test(sync_models = [Post])]
async fn nested_and_or(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;
    let rows = Post::objects()
        // PR3: portable predicates (`PortablePredicate<T>`) compose via
        // `&` from the operator matrix instead of the legacy
        // `Condition::and_with` fluent helper. Same SQL shape, same
        // operator precedence; the closure receives the `DjogiField`
        // wrapper and reaches for the post-flip portable surface.
        .filter(|f| f.published().eq(true) & f.view_count().gte(50i32))
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    // alpha (views=100, published) + beta (views=50, published) match; delta
    // (views=25) and gamma (unpublished) do not.
    assert_eq!(rows.len(), 2);
}

#[djogi::djogi_test(sync_models = [Post])]
async fn in_list_and_between(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;

    // PR3: portable IN takes any `IntoIterator<Item = V>` and is named
    // `in_` (not `in_list`) on `DjogiField`. SQL parity is preserved
    // through the portable lowering helpers.
    let by_title = Post::objects()
        .filter(|f| f.title().in_(vec!["alpha".to_string(), "beta".to_string()]))
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

#[djogi::djogi_test(sync_models = [Post])]
async fn filter_struct_matches_closure_results(mut ctx: djogi::DjogiContext) {
    // Task 8 parity check: `filter_struct` (programmatic) and `filter`
    // (closure) must produce structurally equivalent filters for the
    // same set of lookups.
    use std::collections::BTreeSet;
    seed_posts(&mut ctx).await;

    let closure_rows = Post::objects()
        // PR3: portable predicates (`PortablePredicate<T>`) compose via
        // `&` from the operator matrix instead of the legacy
        // `Condition::and_with` fluent helper. Same SQL shape, same
        // operator precedence; the closure receives the `DjogiField`
        // wrapper and reaches for the post-flip portable surface.
        .filter(|f| f.published().eq(true) & f.view_count().gte(50i32))
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

#[djogi::djogi_test(sync_models = [Post])]
async fn filter_struct_empty_is_identity(mut ctx: djogi::DjogiContext) {
    // A filter with zero setters should not AND anything onto the
    // queryset — terminal fetch should see every row `seed_posts`
    // inserted.
    seed_posts(&mut ctx).await;

    let empty = PostFilter::new();
    let rows = Post::objects()
        .filter_struct(empty)
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(rows.len(), 4);
}

#[djogi::djogi_test(sync_models = [Post])]
async fn filter_struct_single_clause_unwraps_to_leaf(mut ctx: djogi::DjogiContext) {
    // A single-clause filter should emit SQL equivalent to a bare leaf.
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

#[djogi::djogi_test(sync_models = [Post])]
async fn bulk_update_sets_values_and_returns_count(mut ctx: djogi::DjogiContext) {
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

#[djogi::djogi_test(sync_models = [Post])]
async fn bulk_update_none_short_circuits(mut ctx: djogi::DjogiContext) {
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

#[djogi::djogi_test(sync_models = [Post])]
async fn bulk_delete_removes_rows_and_returns_count(mut ctx: djogi::DjogiContext) {
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

#[djogi::djogi_test(sync_models = [Post])]
async fn bulk_update_stamps_updated_at(mut ctx: djogi::DjogiContext) {
    // Contract: bulk update must always stamp `updated_at = now()`, even
    // when the user did not set it themselves.
    seed_posts(&mut ctx).await;

    let initial_rows = Post::objects().fetch_all(&mut ctx).await.unwrap();
    assert_eq!(
        initial_rows
            .iter()
            .filter(|row| row.updated_at == row.created_at)
            .count(),
        4
    );

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

    let bumped = Post::objects()
        .filter(|f| f.title().eq("alpha".to_string()))
        .fetch_one(&mut ctx)
        .await
        .unwrap();
    assert!(
        bumped.updated_at > bumped.created_at,
        "bulk update must stamp updated_at = now() on touched rows"
    );

    let untouched = Post::objects()
        .exclude(|f| f.title().eq("alpha".to_string()))
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(
        untouched
            .iter()
            .filter(|row| row.updated_at == row.created_at)
            .count(),
        3
    );
}

#[djogi::djogi_test(sync_models = [Post])]
async fn distinct_on_and_plain(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;
    create_post(&mut ctx, "dup", "x", true, 1, None)
        .await
        .unwrap();
    create_post(&mut ctx, "dup", "y", true, 2, None)
        .await
        .unwrap();

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
#[djogi::djogi_test(sync_models = [Post])]
async fn in_list_empty_returns_zero_rows(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;

    let rows = Post::objects()
        .filter(|f| f.id().in_(Vec::<HeerId>::new()))
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(rows.len(), 0, "empty IN list must match zero rows");

    let n = Post::objects()
        .filter(|f| f.id().in_(Vec::<HeerId>::new()))
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 0, "empty IN list count must be 0");
}

/// `not_in_list(vec![])` must match every row.
#[djogi::djogi_test(sync_models = [Post])]
async fn not_in_list_empty_returns_all_rows(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;

    let n = Post::objects()
        .filter(|f| f.id().not_in(Vec::<HeerId>::new()))
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 4, "empty NOT IN list must match every row");
}

/// `contains(...)` must escape LIKE wildcards (`%`, `_`, `\`) in user input.
#[djogi::djogi_test(sync_models = [Post])]
async fn string_contains_escapes_percent_and_underscore(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;

    // Add the target row (contains both wildcard characters verbatim) plus
    // two negative-control rows that a broken escape would falsely match.
    create_post(&mut ctx, "50% off_deal", "b", true, 1, None)
        .await
        .unwrap();
    create_post(&mut ctx, "50 off regular", "b", true, 1, None)
        .await
        .unwrap();
    create_post(&mut ctx, "xdeal", "b", true, 1, None)
        .await
        .unwrap();

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
#[djogi::djogi_test(sync_models = [Post])]
async fn exclude_wraps_in_not(mut ctx: djogi::DjogiContext) {
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
#[djogi::djogi_test(sync_models = [Post])]
async fn order_by_stacks_across_multiple_calls(mut ctx: djogi::DjogiContext) {
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
#[djogi::djogi_test(sync_models = [Post])]
async fn order_by_nulls_first_renders(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;

    create_post(&mut ctx, "nullrow", "b", true, 0, None)
        .await
        .unwrap();

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
#[djogi::djogi_test(sync_models = [Post])]
async fn bulk_update_empty_assignments_short_circuits(mut ctx: djogi::DjogiContext) {
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

    assert_eq!(
        rows.iter()
            .filter(|row| row.updated_at == row.created_at)
            .count(),
        4,
        "updated_at must still equal created_at — no UPDATE fired"
    );
}

#[djogi::djogi_test(sync_models = [Post])]
async fn bulk_update_limit_is_rejected_with_validation_error(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;

    let err = Post::objects()
        .limit(1)
        .update(|f| f.view_count().set(999i32))
        .execute(&mut ctx)
        .await
        .expect_err("limit on bulk update must be rejected");
    match err {
        DjogiError::Validation(msg) => {
            assert!(
                msg.contains("limit"),
                "validation should mention limit: {msg}"
            );
        }
        other => panic!("expected DjogiError::Validation, got {other:?}"),
    }

    let bumped = Post::objects()
        .filter(|f| f.view_count().eq(999i32))
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(bumped, 0, "rejected update must not mutate any row");
}

#[djogi::djogi_test(sync_models = [Post])]
async fn bulk_update_order_by_is_rejected_with_validation_error(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;

    let err = Post::objects()
        .order_by(|f| f.title().asc())
        .update(|f| f.view_count().set(999i32))
        .execute(&mut ctx)
        .await
        .expect_err("explicit order_by on bulk update must be rejected");
    match err {
        DjogiError::Validation(msg) => {
            assert!(
                msg.contains("order_by"),
                "validation should mention order_by: {msg}"
            );
        }
        other => panic!("expected DjogiError::Validation, got {other:?}"),
    }

    let bumped = Post::objects()
        .filter(|f| f.view_count().eq(999i32))
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(bumped, 0, "rejected update must not mutate any row");
}

// ── Phase 8.5 djogi#180 — PG18 OLD/NEW bulk RETURNING integration tests ──

#[djogi::djogi_test(sync_models = [Post])]
async fn execute_returning_pairs_returns_old_and_new_for_each_row(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;

    // Update only the three published rows.
    let pairs = Post::objects()
        .filter(|f| f.published().eq(true))
        .update(|f| f.view_count().set(999i32))
        .execute_returning_pairs(&mut ctx)
        .await
        .expect("execute_returning_pairs should succeed");

    assert_eq!(pairs.len(), 3, "expected one pair per affected row");

    for pair in &pairs {
        // id and created_at must not change across an update.
        assert_eq!(pair.old.id, pair.new.id, "id must be stable");
        assert_eq!(
            pair.old.created_at, pair.new.created_at,
            "created_at must be stable"
        );
        // The old side reflects the seeded view_count values (not 999).
        assert_ne!(
            pair.old.view_count, 999,
            "old view_count must be the pre-update seeded value"
        );
        // The new side reflects the new value.
        assert_eq!(pair.new.view_count, 999, "new view_count must be 999");
        // updated_at must not regress.
        assert!(
            pair.new.updated_at >= pair.old.updated_at,
            "new.updated_at must not be before old.updated_at"
        );
    }
}

#[djogi::djogi_test(sync_models = [ReturningPairLongAliasBulk])]
async fn execute_returning_pairs_handles_boundary_and_collision_oriented_aliases(
    mut ctx: djogi::DjogiContext,
) {
    assert_eq!(
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx".len(),
        50,
        "first long column name should be 50 chars"
    );
    assert_eq!(
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxa".len(),
        52,
        "second long column name should be 52 chars"
    );
    assert_eq!(
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxb".len(),
        52,
        "third long column name should be 52 chars"
    );

    let _rows = seed_returning_pair_long_alias_bulk_rows(&mut ctx)
        .await
        .expect("seed rows with long aliased fields");

    let pairs = ReturningPairLongAliasBulk::objects()
        .update(|f| {
            f.xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxa()
                .set(999i32)
        })
        .execute_returning_pairs(&mut ctx)
        .await
        .expect("execute_returning_pairs should decode long-field aliases");

    assert_eq!(pairs.len(), 3, "expected one pair per updated row");

    for pair in &pairs {
        assert_eq!(pair.old.id, pair.new.id);
        assert_eq!(pair.old.created_at, pair.new.created_at);
        assert_eq!(
            pair.new
                .xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxa,
            999,
            "updated long field should reflect new value"
        );
        assert_ne!(
            pair.old
                .xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxa,
            pair.new
                .xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxa,
            "old/new value for updated field must differ"
        );
        assert_eq!(
            pair.old.xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx,
            pair.new.xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx,
            "untouched fields should still preserve per-row identity"
        );
        assert_eq!(
            pair.old
                .xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxb,
            pair.new
                .xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxb,
            "untouched fields should still preserve per-row identity"
        );
    }

    let mut pairs_by_initial = pairs;
    pairs_by_initial
        .sort_by_key(|pair| pair.old.xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx);
    assert_eq!(
        pairs_by_initial[0]
            .old
            .xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx,
        1,
        "boundary fixture order assertion"
    );
    assert_eq!(
        pairs_by_initial[0]
            .old
            .xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxa,
        11,
        "first pair should come from seeded row 11/21"
    );
    assert_eq!(
        pairs_by_initial[0]
            .old
            .xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxb,
        21,
        "first pair should come from seeded row 11/21"
    );
}

#[djogi::djogi_test(sync_models = [BulkOutboxEvtRow])]
async fn execute_returning_pairs_events_model_emits_save_outbox_per_pair(
    mut ctx: djogi::DjogiContext,
) {
    let rows = seed_bulk_outbox_evt_rows(&mut ctx)
        .await
        .expect("seed bulk outbox events rows");

    djogi::testing::clear_outbox_for_test(&mut ctx, "phase2_bulk_outbox_evt_row_outbox")
        .await
        .expect("clear outbox rows");

    let pairs = BulkOutboxEvtRow::objects()
        .update(|f| f.score().set(42i32))
        .execute_returning_pairs(&mut ctx)
        .await
        .expect("execute_returning_pairs should emit save outbox rows");

    assert_eq!(pairs.len(), rows.len(), "one pair per updated row");

    let outbox_rows =
        djogi::testing::outbox_rows_for_test(&mut ctx, "phase2_bulk_outbox_evt_row_outbox")
            .await
            .expect("read phase2_bulk_outbox_evt_row_outbox rows");

    assert_eq!(
        outbox_rows.len(),
        rows.len(),
        "one outbox row per updated row"
    );

    for outbox_row in outbox_rows {
        assert_eq!(
            outbox_row.action, "save",
            "bulk save must emit save action rows"
        );

        let pair = pairs
            .iter()
            .find(|pair| pair.new.id.to_string() == outbox_row.row_id)
            .expect("outbox row id should match a returned pair");
        let payload = outbox_row
            .payload
            .as_object()
            .expect("outbox payload must be object");
        assert_eq!(
            payload["tag"],
            serde_json::Value::String(pair.new.tag.clone())
        );
        assert_eq!(payload["score"], serde_json::Value::from(pair.new.score));
    }
}

#[djogi::djogi_test(sync_models = [BulkOutboxNoEvtRow])]
async fn execute_returning_pairs_non_events_model_does_not_require_outbox_plumbing(
    mut ctx: djogi::DjogiContext,
) {
    let rows = seed_bulk_outbox_no_evt_rows(&mut ctx)
        .await
        .expect("seed non-events rows");

    let pairs = BulkOutboxNoEvtRow::objects()
        .update(|f| f.score().set(99i32))
        .execute_returning_pairs(&mut ctx)
        .await
        .expect("non-events bulk execute_returning_pairs should succeed");

    assert_eq!(pairs.len(), rows.len(), "one pair per updated row");
    assert!(
        pairs.iter().all(|pair| pair.new.score == 99),
        "all returned pairs must have updated score"
    );
    assert!(
        !BulkOutboxNoEvtRow::descriptor().has_outbox,
        "non-events model must not have outbox"
    );
}

#[djogi::djogi_test(sync_models = [BulkHooksRow])]
async fn execute_returning_pairs_does_not_dispatch_lifecycle_hooks(mut ctx: djogi::DjogiContext) {
    BULK_BEFORE_SAVE_CALLS.store(0, Ordering::SeqCst);
    BULK_AFTER_SAVE_CALLS.store(0, Ordering::SeqCst);

    let rows = seed_bulk_hooks_rows(&mut ctx)
        .await
        .expect("seed hook-enabled rows");

    let pairs = BulkHooksRow::objects()
        .update(|f| f.score().set(777i32))
        .execute_returning_pairs(&mut ctx)
        .await
        .expect("bulk execute_returning_pairs should succeed");

    assert_eq!(pairs.len(), rows.len(), "one pair per updated row");
    assert_eq!(
        BULK_BEFORE_SAVE_CALLS.load(Ordering::SeqCst),
        0,
        "bulk execute_returning_pairs must not run before_save hooks"
    );
    assert_eq!(
        BULK_AFTER_SAVE_CALLS.load(Ordering::SeqCst),
        0,
        "bulk execute_returning_pairs must not run after_save hooks"
    );
}

#[djogi::djogi_test(sync_models = [Post])]
async fn execute_returning_pairs_none_queryset_returns_empty(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;

    let pairs = Post::objects()
        .none()
        .update(|f| f.view_count().set(0i32))
        .execute_returning_pairs(&mut ctx)
        .await
        .expect("execute_returning_pairs on none() should return empty");

    assert!(pairs.is_empty(), "none() queryset must return empty pairs");
}

#[djogi::djogi_test(sync_models = [Post])]
async fn execute_returning_pairs_empty_assignments_returns_empty(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;

    let pairs = Post::objects()
        .update(|_| Vec::<djogi::UpdateAssignment>::new())
        .execute_returning_pairs(&mut ctx)
        .await
        .expect("empty assignments should return empty pairs");

    assert!(
        pairs.is_empty(),
        "empty assignment list must return empty pairs without SQL"
    );
}

#[djogi::djogi_test(sync_models = [Post])]
async fn execute_returning_pairs_limit_is_rejected_with_validation_error(
    mut ctx: djogi::DjogiContext,
) {
    seed_posts(&mut ctx).await;

    let err = Post::objects()
        .limit(1)
        .update(|f| f.view_count().set(777i32))
        .execute_returning_pairs(&mut ctx)
        .await
        .expect_err("limit on execute_returning_pairs must be rejected");
    match err {
        DjogiError::Validation(msg) => {
            assert!(
                msg.contains("limit"),
                "validation should mention limit: {msg}"
            );
        }
        other => panic!("expected DjogiError::Validation, got {other:?}"),
    }

    let bumped = Post::objects()
        .filter(|f| f.view_count().eq(777i32))
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(bumped, 0, "rejected update must not mutate any row");
}

#[djogi::djogi_test(sync_models = [Post])]
async fn bulk_delete_returning_returns_deleted_rows(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;

    // Delete only unpublished rows (just "gamma").
    let deleted = Post::objects()
        .filter(|f| f.published().eq(false))
        .delete_returning(&mut ctx)
        .await
        .expect("delete_returning should succeed");

    assert_eq!(deleted.len(), 1, "expected 1 deleted row");
    assert_eq!(deleted[0].title, "gamma");
    assert!(!deleted[0].published);

    // Confirm the rows are actually gone.
    let remaining = Post::objects().count(&mut ctx).await.unwrap();
    assert_eq!(remaining, 3, "3 published rows should remain");
}

#[djogi::djogi_test(sync_models = [Post])]
async fn bulk_delete_returning_none_queryset_returns_empty(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;

    let deleted = Post::objects()
        .none()
        .delete_returning(&mut ctx)
        .await
        .expect("delete_returning on none() should return empty");

    assert!(deleted.is_empty(), "none() delete_returning must be empty");

    // No rows deleted.
    let remaining = Post::objects().count(&mut ctx).await.unwrap();
    assert_eq!(remaining, 4, "no rows should have been deleted");
}

#[djogi::djogi_test(sync_models = [Post])]
async fn bulk_delete_returning_preserves_snapshot_values(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;

    // Delete all rows and verify the returned snapshots match the seeded data.
    let mut deleted = Post::objects()
        .delete_returning(&mut ctx)
        .await
        .expect("delete_returning all rows should succeed");

    assert_eq!(deleted.len(), 4, "all 4 rows should be returned");

    // Sort by title for deterministic comparison.
    deleted.sort_by(|a, b| a.title.cmp(&b.title));
    let titles: Vec<&str> = deleted.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(titles, ["alpha", "beta", "delta", "gamma"]);

    // Table should be empty.
    let remaining = Post::objects().count(&mut ctx).await.unwrap();
    assert_eq!(remaining, 0, "all rows should be deleted");
}

#[djogi::djogi_test(sync_models = [Post])]
async fn bulk_delete_limit_is_rejected_with_validation_error(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;

    let err = Post::objects()
        .limit(1)
        .delete(&mut ctx)
        .await
        .expect_err("limit on bulk delete must be rejected");
    match err {
        DjogiError::Validation(msg) => {
            assert!(
                msg.contains("limit"),
                "validation should mention limit: {msg}"
            );
        }
        other => panic!("expected DjogiError::Validation, got {other:?}"),
    }

    let remaining = Post::objects().count(&mut ctx).await.unwrap();
    assert_eq!(remaining, 4, "rejected delete must not remove rows");
}

#[djogi::djogi_test(sync_models = [Post])]
async fn bulk_delete_order_by_is_rejected_with_validation_error(mut ctx: djogi::DjogiContext) {
    seed_posts(&mut ctx).await;

    let err = Post::objects()
        .order_by(|f| f.title().asc())
        .delete(&mut ctx)
        .await
        .expect_err("explicit order_by on bulk delete must be rejected");
    match err {
        DjogiError::Validation(msg) => {
            assert!(
                msg.contains("order_by"),
                "validation should mention order_by: {msg}"
            );
        }
        other => panic!("expected DjogiError::Validation, got {other:?}"),
    }

    let remaining = Post::objects().count(&mut ctx).await.unwrap();
    assert_eq!(remaining, 4, "rejected delete must not remove rows");
}

#[djogi::djogi_test(sync_models = [Post])]
async fn bulk_delete_returning_limit_is_rejected_with_validation_error(
    mut ctx: djogi::DjogiContext,
) {
    seed_posts(&mut ctx).await;

    let err = Post::objects()
        .limit(1)
        .delete_returning(&mut ctx)
        .await
        .expect_err("limit on delete_returning must be rejected");
    match err {
        DjogiError::Validation(msg) => {
            assert!(
                msg.contains("limit"),
                "validation should mention limit: {msg}"
            );
        }
        other => panic!("expected DjogiError::Validation, got {other:?}"),
    }

    let remaining = Post::objects().count(&mut ctx).await.unwrap();
    assert_eq!(
        remaining, 4,
        "rejected delete_returning must not remove rows"
    );
}
