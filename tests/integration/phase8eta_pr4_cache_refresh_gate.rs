// Phase 8eta PR4 integration coverage for the portable cache/refresh gate.
//
// These tests pin the boundary introduced after the raw-Sassi ingress removal:
// cache and refresh may only accept querysets whose complete Q<T> tree reduces
// to Djogi-provenanced portable predicates. Ordinary PostgreSQL queries remain
// valid, but SQL-only predicates must not enter Punnu-backed cache state.

use djogi::prelude::*;
use djogi::query::PortablePredicateError;

#[model(table = "phase8eta_pr4_cache_refresh_rows", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct CacheRefreshGateRow {
    pub label: String,
    pub active: bool,
    pub ratings: Vec<i32>,
}

fn assert_cache_invalid_condition(err: PortablePredicateError) {
    assert!(
        matches!(err, PortablePredicateError::CacheInvalidNode { kind } if kind == "Condition"),
        "expected SQL-only Condition to be rejected at portable boundary; got {err:?}",
    );
}

fn assert_unsupported_field_type(err: PortablePredicateError, expected_field: &'static str) {
    assert!(
        matches!(
            err,
            PortablePredicateError::UnsupportedFieldType { field } if field == expected_field
        ),
        "expected unsupported field type for {expected_field}; got {err:?}",
    );
}

#[djogi::djogi_test(sync_models = [CacheRefreshGateRow])]
async fn cache_rejects_pg_specific_predicate(mut ctx: djogi::DjogiContext) {
    let punnu = ctx
        .punnu::<CacheRefreshGateRow>()
        .expect("punnu registered for CacheRefreshGateRow");

    let result = CacheRefreshGateRow::objects()
        .filter(|f| f.label().explicit_pg_predicate().contains("alpha"))
        .cache(&punnu);

    let err = match result {
        Ok(_) => panic!("cache gate must reject SQL-only PostgreSQL predicate"),
        Err((_queryset, err)) => err,
    };
    assert_cache_invalid_condition(err);

    let rows = CacheRefreshGateRow::objects()
        .none()
        .cache(&punnu)
        .expect("none() must satisfy the portable false cache gate")
        .fetch_all(&mut ctx)
        .await
        .expect("empty cache-bound queryset should fetch successfully");

    assert!(
        rows.is_empty(),
        "none().cache(...).fetch_all() must return no rows"
    );
    assert_eq!(
        punnu.len(),
        0,
        "none().cache(...) must not insert rows into the Punnu",
    );
}

#[djogi::djogi_test(sync_models = [CacheRefreshGateRow])]
async fn refresh_rejects_pg_specific_predicate(mut ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");
    let punnu = ctx
        .punnu::<CacheRefreshGateRow>()
        .expect("punnu registered for CacheRefreshGateRow");
    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    let result = CacheRefreshGateRow::objects()
        .filter(|f| f.label().explicit_pg_predicate().contains("alpha"))
        .refresh_into(&punnu, pool, auth);

    let err = match result {
        Ok(_) => panic!("refresh gate must reject SQL-only PostgreSQL predicate"),
        Err((_queryset, err)) => err,
    };
    assert_cache_invalid_condition(err);

    let _ = &mut ctx;
}

#[djogi::djogi_test(sync_models = [CacheRefreshGateRow])]
async fn cache_and_refresh_reject_unsupported_root_field_predicate(mut ctx: djogi::DjogiContext) {
    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");
    let punnu = ctx
        .punnu::<CacheRefreshGateRow>()
        .expect("punnu registered for CacheRefreshGateRow");
    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    let cache_err = match CacheRefreshGateRow::objects()
        .filter(|f| f.ratings().eq(vec![1, 2, 3]))
        .cache(&punnu)
    {
        Ok(_) => panic!("cache gate must reject unsupported array root field predicate"),
        Err((_queryset, err)) => err,
    };
    assert_unsupported_field_type(cache_err, "ratings");

    let refresh_err = match CacheRefreshGateRow::objects()
        .filter(|f| f.ratings().eq(vec![1, 2, 3]))
        .refresh_into(&punnu, pool, auth)
    {
        Ok(_) => panic!("refresh gate must reject unsupported array root field predicate"),
        Err((_queryset, err)) => err,
    };
    assert_unsupported_field_type(refresh_err, "ratings");

    let _ = &mut ctx;
}

#[djogi::djogi_test(sync_models = [CacheRefreshGateRow])]
async fn refresh_none_remains_empty_across_delta_ticks(mut ctx: djogi::DjogiContext) {
    CacheRefreshGateRow::create(
        &mut ctx,
        CacheRefreshGateRow {
            label: "initial".into(),
            active: true,
            ..Default::default()
        },
    )
    .await
    .expect("create initial row");

    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");
    let punnu = ctx
        .punnu::<CacheRefreshGateRow>()
        .expect("punnu registered for CacheRefreshGateRow");
    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    let handle = CacheRefreshGateRow::objects()
        .none()
        .refresh_into(&punnu, pool, auth)
        .expect("none() must satisfy the portable false refresh gate");

    let tick_1 = handle
        .update()
        .await
        .expect("empty refresh first tick must succeed");
    assert_eq!(
        tick_1.applied, 0,
        "none().refresh_into(...).update() must not apply source rows",
    );
    assert_eq!(
        punnu.len(),
        0,
        "none().refresh_into(...) must not insert rows into the Punnu",
    );

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let later = CacheRefreshGateRow::create(
        &mut ctx,
        CacheRefreshGateRow {
            label: "later".into(),
            active: true,
            ..Default::default()
        },
    )
    .await
    .expect("create later row");

    let tick_2 = handle
        .update()
        .await
        .expect("empty refresh delta tick must succeed");
    assert_eq!(
        tick_2.applied, 0,
        "none().refresh_into(...) must remain empty after later source changes",
    );
    assert!(
        punnu.get(&later.id).is_none(),
        "delta ticks from a none() refresh must not leak later source rows into Punnu",
    );
    assert_eq!(
        punnu.len(),
        0,
        "empty refresh subscription must leave the Punnu empty across ticks",
    );
}

#[djogi::djogi_test(sync_models = [CacheRefreshGateRow])]
async fn refresh_full_tick_pushes_portable_filter_and_source_watermark(
    mut ctx: djogi::DjogiContext,
) {
    let visible = CacheRefreshGateRow::create(
        &mut ctx,
        CacheRefreshGateRow {
            label: "visible".into(),
            active: true,
            ..Default::default()
        },
    )
    .await
    .expect("create visible row");

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let hidden = CacheRefreshGateRow::create(
        &mut ctx,
        CacheRefreshGateRow {
            label: "hidden".into(),
            active: false,
            ..Default::default()
        },
    )
    .await
    .expect("create hidden row");

    assert!(
        hidden.updated_at >= visible.updated_at,
        "fixture must give the hidden row the table high watermark",
    );

    let pool = ctx
        .share_pool()
        .expect("djogi_test context must have a pool");
    let punnu = ctx
        .punnu::<CacheRefreshGateRow>()
        .expect("punnu registered for CacheRefreshGateRow");
    let auth =
        djogi::auth::AuthContext::new(djogi::HeerId::from_i64(1).expect("HeerId(1) is valid"));

    let handle = CacheRefreshGateRow::objects()
        .filter(|f| f.active().eq(true))
        .refresh_into(&punnu, pool, auth)
        .expect("portable bool predicate must satisfy refresh gate");

    let tick = handle
        .update()
        .await
        .expect("filtered refresh tick must succeed");

    assert_eq!(
        tick.applied, 1,
        "full baseline tick must push active=true into SQL and apply only the visible row",
    );
    assert!(
        punnu.get(&visible.id).is_some(),
        "matching row must be inserted into Punnu",
    );
    assert!(
        punnu.get(&hidden.id).is_none(),
        "non-matching row must not be inserted during the filtered full baseline tick",
    );
    assert_eq!(
        handle.watermark(),
        Some(hidden.updated_at),
        "refresh must advance to the unfiltered table high watermark, not just the max matching row",
    );
}
