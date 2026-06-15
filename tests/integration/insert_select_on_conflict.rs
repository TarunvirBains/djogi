use djogi::cache::Cacheable;
use djogi::prelude::*;

#[model(table = "oc_sources", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct OcSource {
    pub slug: String,
    pub hits: i32,
    pub maybe_hits: Option<i32>,
    pub published: bool,
}

#[model(
    table = "oc_targets",
    pk = HeerIdRecencyBiased,
    indexes(unique(fields = [slug], name = "oc_targets_slug_key"))
)]
#[derive(Debug, Clone)]
pub struct OcTarget {
    pub slug: String,
    pub hits: i32,
    pub maybe_hits: Option<i32>,
    pub published: bool,
}

#[model(
    table = "oc_partial_targets",
    pk = HeerIdRecencyBiased,
    indexes(unique(fields = [slug], where = "published", name = "oc_partial_published_key"))
)]
#[derive(Debug, Clone)]
pub struct OcPartialTarget {
    pub slug: String,
    pub hits: i32,
    pub maybe_hits: Option<i32>,
    pub published: bool,
}

async fn seed_source(ctx: &mut djogi::DjogiContext, slug: &str, hits: i32, published: bool) {
    OcSource::create(
        ctx,
        OcSource {
            slug: slug.to_string(),
            hits,
            maybe_hits: None,
            published,
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

async fn seed_target(ctx: &mut djogi::DjogiContext, slug: &str, hits: i32) {
    OcTarget::create(
        ctx,
        OcTarget {
            slug: slug.to_string(),
            hits,
            maybe_hits: None,
            published: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

async fn seed_source_with_nullable(
    ctx: &mut djogi::DjogiContext,
    slug: &str,
    hits: i32,
    maybe_hits: Option<i32>,
    published: bool,
) {
    OcSource::create(
        ctx,
        OcSource {
            slug: slug.to_string(),
            hits,
            maybe_hits,
            published,
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

async fn seed_target_with_nullable(
    ctx: &mut djogi::DjogiContext,
    slug: &str,
    hits: i32,
    maybe_hits: Option<i32>,
) {
    OcTarget::create(
        ctx,
        OcTarget {
            slug: slug.to_string(),
            hits,
            maybe_hits,
            published: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

#[djogi::djogi_test(sync_models = [OcSource, OcTarget])]
async fn do_nothing_with_columns_skips_conflicts(mut ctx: djogi::DjogiContext) {
    seed_target(&mut ctx, "alpha", 1).await;
    seed_source(&mut ctx, "alpha", 999, true).await;
    seed_source(&mut ctx, "beta", 2, true).await;

    let n = OcSource::objects()
        .insert_into::<OcTarget, _, _>(|t, s| {
            vec![
                t.slug().copy_from(s.slug().as_insert_source()),
                t.hits().copy_from(s.hits().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
            ]
        })
        .on_conflict_do_nothing(ConflictTarget::columns([OcTarget::fields().slug()]))
        .execute(&mut ctx)
        .await
        .unwrap();

    assert_eq!(n, 1);
    let rows = OcTarget::objects()
        .order_by(|f| f.slug().asc())
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].slug, "alpha");
    assert_eq!(rows[0].hits, 1);
    assert_eq!(rows[1].slug, "beta");
}

#[djogi::djogi_test(sync_models = [OcSource, OcTarget])]
async fn do_nothing_with_constraint_and_bare_target_work(mut ctx: djogi::DjogiContext) {
    seed_target(&mut ctx, "alpha", 1).await;
    seed_source(&mut ctx, "alpha", 5, true).await;
    seed_source(&mut ctx, "beta", 2, true).await;

    let n = OcSource::objects()
        .insert_into::<OcTarget, _, _>(|t, s| {
            vec![
                t.slug().copy_from(s.slug().as_insert_source()),
                t.hits().copy_from(s.hits().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
            ]
        })
        .on_conflict_do_nothing(ConflictTarget::constraint("oc_targets_slug_key"))
        .execute(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 1);

    let bare_n = OcSource::objects()
        .insert_into::<OcTarget, _, _>(|t, s| {
            vec![
                t.slug().copy_from(s.slug().as_insert_source()),
                t.hits().copy_from(s.hits().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
            ]
        })
        .on_conflict_do_nothing(ConflictTarget::<OcTarget>::none())
        .execute(&mut ctx)
        .await
        .unwrap();
    assert_eq!(bare_n, 0);
}

#[djogi::djogi_test(sync_models = [OcSource, OcTarget])]
async fn do_update_overwrites_and_accumulates(mut ctx: djogi::DjogiContext) {
    seed_target(&mut ctx, "alpha", 1).await;
    seed_source(&mut ctx, "alpha", 5, true).await;
    seed_source(&mut ctx, "beta", 2, true).await;

    let overwrite_n = OcSource::objects()
        .insert_into::<OcTarget, _, _>(|t, s| {
            vec![
                t.slug().copy_from(s.slug().as_insert_source()),
                t.hits().copy_from(s.hits().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
            ]
        })
        .on_conflict_do_update(ConflictTarget::columns([OcTarget::fields().slug()]), |t| {
            vec![t.hits().conflict_set(t.hits().excluded())]
        })
        .execute(&mut ctx)
        .await
        .unwrap();
    assert_eq!(overwrite_n, 2);

    let alpha = OcTarget::objects()
        .filter(|f| f.slug().eq("alpha".to_string()))
        .fetch_one(&mut ctx)
        .await
        .unwrap();
    assert_eq!(alpha.hits, 5);

    OcSource::objects().delete(&mut ctx).await.unwrap();
    seed_source(&mut ctx, "alpha", 3, true).await;

    let expr_n = OcSource::objects()
        .filter(|f| f.slug().eq("alpha".to_string()) & f.hits().eq(3))
        .insert_into::<OcTarget, _, _>(|t, s| {
            vec![
                t.slug().copy_from(s.slug().as_insert_source()),
                t.hits().copy_from(s.hits().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
            ]
        })
        .on_conflict_do_update(ConflictTarget::columns([OcTarget::fields().slug()]), |t| {
            vec![t.hits().conflict_set_expr(
                t.hits().as_conflict_expr() + t.hits().excluded().into_conflict_expr(),
            )]
        })
        .execute(&mut ctx)
        .await
        .unwrap();
    assert_eq!(expr_n, 1);

    let alpha_after = OcTarget::objects()
        .filter(|f| f.slug().eq("alpha".to_string()))
        .fetch_one(&mut ctx)
        .await
        .unwrap();
    assert_eq!(alpha_after.hits, 8);
}

#[djogi::djogi_test(sync_models = [OcSource, OcTarget])]
async fn do_update_where_skips_when_guard_false(mut ctx: djogi::DjogiContext) {
    seed_target(&mut ctx, "alpha", 100).await;
    seed_target(&mut ctx, "gamma", 1).await;
    seed_source(&mut ctx, "alpha", 5, true).await;
    seed_source(&mut ctx, "gamma", 50, true).await;

    OcSource::objects()
        .insert_into::<OcTarget, _, _>(|t, s| {
            vec![
                t.slug().copy_from(s.slug().as_insert_source()),
                t.hits().copy_from(s.hits().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
            ]
        })
        .on_conflict_do_update_where(
            ConflictTarget::columns([OcTarget::fields().slug()]),
            |t| vec![t.hits().conflict_set(t.hits().excluded())],
            |t| t.hits().excluded().conflict_gt(t.hits()),
        )
        .execute(&mut ctx)
        .await
        .unwrap();

    let alpha = OcTarget::objects()
        .filter(|f| f.slug().eq("alpha".to_string()))
        .fetch_one(&mut ctx)
        .await
        .unwrap();
    assert_eq!(alpha.hits, 100);

    let gamma = OcTarget::objects()
        .filter(|f| f.slug().eq("gamma".to_string()))
        .fetch_one(&mut ctx)
        .await
        .unwrap();
    assert_eq!(gamma.hits, 50);
}

#[djogi::djogi_test(sync_models = [OcSource, OcTarget])]
async fn do_update_where_with_conflict_is_null_updates_only_empty_targets(
    mut ctx: djogi::DjogiContext,
) {
    seed_target_with_nullable(&mut ctx, "alpha", 1, Some(100)).await;
    seed_target_with_nullable(&mut ctx, "beta", 1, None).await;
    seed_source_with_nullable(&mut ctx, "alpha", 5, Some(5), true).await;
    seed_source_with_nullable(&mut ctx, "beta", 6, Some(6), true).await;

    OcSource::objects()
        .insert_into::<OcTarget, _, _>(|t, s| {
            vec![
                t.slug().copy_from(s.slug().as_insert_source()),
                t.hits().copy_from(s.hits().as_insert_source()),
                t.maybe_hits().copy_from(s.maybe_hits().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
            ]
        })
        .on_conflict_do_update_where(
            ConflictTarget::columns([OcTarget::fields().slug()]),
            |t| vec![t.maybe_hits().conflict_set(t.maybe_hits().excluded())],
            |t| t.maybe_hits().conflict_is_null(),
        )
        .execute(&mut ctx)
        .await
        .unwrap();

    let rows = OcTarget::objects()
        .order_by(|f| f.slug().asc())
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(rows[0].slug, "alpha");
    assert_eq!(rows[0].maybe_hits, Some(100));
    assert_eq!(rows[1].slug, "beta");
    assert_eq!(rows[1].maybe_hits, Some(6));
}

#[djogi::djogi_test(sync_models = [OcSource, OcTarget])]
async fn do_update_with_conflict_coalesce_excluded_keeps_existing_non_null_values(
    mut ctx: djogi::DjogiContext,
) {
    seed_target_with_nullable(&mut ctx, "alpha", 1, Some(100)).await;
    seed_target_with_nullable(&mut ctx, "beta", 1, None).await;
    seed_source_with_nullable(&mut ctx, "alpha", 5, Some(5), true).await;
    seed_source_with_nullable(&mut ctx, "beta", 6, Some(6), true).await;

    OcSource::objects()
        .insert_into::<OcTarget, _, _>(|t, s| {
            vec![
                t.slug().copy_from(s.slug().as_insert_source()),
                t.hits().copy_from(s.hits().as_insert_source()),
                t.maybe_hits().copy_from(s.maybe_hits().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
            ]
        })
        .on_conflict_do_update(ConflictTarget::columns([OcTarget::fields().slug()]), |t| {
            vec![t
                .maybe_hits()
                .conflict_set_expr(t.maybe_hits().conflict_coalesce_excluded())]
        })
        .execute(&mut ctx)
        .await
        .unwrap();

    let rows = OcTarget::objects()
        .order_by(|f| f.slug().asc())
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(rows[0].slug, "alpha");
    assert_eq!(rows[0].maybe_hits, Some(100));
    assert_eq!(rows[1].slug, "beta");
    assert_eq!(rows[1].maybe_hits, Some(6));
}

#[djogi::djogi_test(sync_models = [OcSource, OcTarget])]
async fn do_update_where_with_conflict_is_not_null_updates_only_filled_targets(
    mut ctx: djogi::DjogiContext,
) {
    seed_target_with_nullable(&mut ctx, "alpha", 1, Some(100)).await;
    seed_target_with_nullable(&mut ctx, "beta", 1, None).await;
    seed_source_with_nullable(&mut ctx, "alpha", 5, Some(5), true).await;
    seed_source_with_nullable(&mut ctx, "beta", 6, Some(6), true).await;

    OcSource::objects()
        .insert_into::<OcTarget, _, _>(|t, s| {
            vec![
                t.slug().copy_from(s.slug().as_insert_source()),
                t.hits().copy_from(s.hits().as_insert_source()),
                t.maybe_hits().copy_from(s.maybe_hits().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
            ]
        })
        .on_conflict_do_update_where(
            ConflictTarget::columns([OcTarget::fields().slug()]),
            |t| vec![t.maybe_hits().conflict_set(t.maybe_hits().excluded())],
            |t| t.maybe_hits().conflict_is_not_null(),
        )
        .execute(&mut ctx)
        .await
        .unwrap();

    let rows = OcTarget::objects()
        .order_by(|f| f.slug().asc())
        .fetch_all(&mut ctx)
        .await
        .unwrap();
    assert_eq!(rows[0].slug, "alpha");
    assert_eq!(rows[0].maybe_hits, Some(5));
    assert_eq!(rows[1].slug, "beta");
    assert_eq!(rows[1].maybe_hits, None);
}

#[djogi::djogi_test(sync_models = [OcSource, OcPartialTarget])]
async fn inference_predicate_matches_partial_index(mut ctx: djogi::DjogiContext) {
    OcPartialTarget::create(
        &mut ctx,
        OcPartialTarget {
            slug: "alpha".to_string(),
            hits: 1,
            published: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    seed_source(&mut ctx, "alpha", 999, true).await;
    seed_source(&mut ctx, "beta", 2, true).await;

    let n = OcSource::objects()
        .insert_into::<OcPartialTarget, _, _>(|t, s| {
            vec![
                t.slug().copy_from(s.slug().as_insert_source()),
                t.hits().copy_from(s.hits().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
            ]
        })
        .on_conflict_do_nothing(
            ConflictTarget::columns([OcPartialTarget::fields().slug()])
                .where_predicate(|t| t.published().conflict_is_true()),
        )
        .execute(&mut ctx)
        .await
        .unwrap();

    assert_eq!(n, 1);
}

#[djogi::djogi_test(sync_models = [OcSource, OcTarget])]
async fn execute_returning_with_conflict_behaves_like_postgres(mut ctx: djogi::DjogiContext) {
    seed_target(&mut ctx, "alpha", 1).await;
    seed_source(&mut ctx, "alpha", 999, true).await;
    seed_source(&mut ctx, "beta", 2, true).await;

    let returned: Vec<OcTarget> = OcSource::objects()
        .order_by(|f| f.slug().asc())
        .insert_into::<OcTarget, _, _>(|t, s| {
            vec![
                t.slug().copy_from(s.slug().as_insert_source()),
                t.hits().copy_from(s.hits().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
            ]
        })
        .on_conflict_do_update(ConflictTarget::columns([OcTarget::fields().slug()]), |t| {
            vec![t.hits().conflict_set(t.hits().excluded())]
        })
        .execute_returning(&mut ctx)
        .await
        .unwrap();
    assert_eq!(returned.len(), 2);

    let returned_skip: Vec<OcTarget> = OcSource::objects()
        .insert_into::<OcTarget, _, _>(|t, s| {
            vec![
                t.slug().copy_from(s.slug().as_insert_source()),
                t.hits().copy_from(s.hits().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
            ]
        })
        .on_conflict_do_nothing(ConflictTarget::columns([OcTarget::fields().slug()]))
        .execute_returning(&mut ctx)
        .await
        .unwrap();
    assert_eq!(returned_skip.len(), 0);
}

#[djogi::djogi_test(sync_models = [OcSource, OcTarget])]
async fn none_short_circuit_still_validates_conflict_clause(mut ctx: djogi::DjogiContext) {
    let err = OcSource::objects()
        .none()
        .insert_into::<OcTarget, _, _>(|t, s| {
            vec![
                t.slug().copy_from(s.slug().as_insert_source()),
                t.hits().copy_from(s.hits().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
            ]
        })
        .on_conflict_do_nothing(ConflictTarget::columns_of(
            ConflictColumns::<OcTarget>::new(),
        ))
        .execute(&mut ctx)
        .await
        .unwrap_err();
    assert!(matches!(err, DjogiError::Validation(ref m) if m.contains("conflict target")));

    let err2 = OcSource::objects()
        .none()
        .insert_into::<OcTarget, _, _>(|t, s| {
            vec![
                t.slug().copy_from(s.slug().as_insert_source()),
                t.hits().copy_from(s.hits().as_insert_source()),
                t.published().copy_from(s.published().as_insert_source()),
            ]
        })
        .on_conflict_do_update(ConflictTarget::columns([OcTarget::fields().slug()]), |_t| {
            Vec::<ConflictUpdate<OcSource, OcTarget>>::new()
        })
        .execute(&mut ctx)
        .await
        .unwrap_err();
    assert!(matches!(err2, DjogiError::Validation(ref m) if m.contains("DO UPDATE SET")));
}
