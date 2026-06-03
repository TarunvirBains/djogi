// Issue #178 — typed MERGE INTO integration tests against a live Postgres.
//
// Pins the live-DB behavior of the new `QuerySet::merge_into` surface.
// Covers basic upsert, soft-delete (sync), and validation rejections.

use djogi::cache::Cacheable;
use djogi::prelude::*;
use djogi::query::MergeWhenCondition;
use std::time::Duration;
use tokio::time::sleep;

#[model(table = "merge_sources", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct MergeSource {
    pub external_id: String,
    pub payload: String,
}

#[model(table = "merge_targets", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct MergeTarget {
    pub external_id: String,
    pub payload: String,
    pub active: bool,
}

async fn seed_source(ctx: &mut djogi::DjogiContext, ext_id: &str, payload: &str) -> MergeSource {
    MergeSource::create(
        ctx,
        MergeSource {
            external_id: ext_id.to_string(),
            payload: payload.to_string(),
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

async fn seed_target(ctx: &mut djogi::DjogiContext, ext_id: &str, payload: &str) -> MergeTarget {
    MergeTarget::create(
        ctx,
        MergeTarget {
            external_id: ext_id.to_string(),
            payload: payload.to_string(),
            active: true,
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

// ── Happy path: Upsert ───────────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [MergeSource, MergeTarget])]
async fn merge_upsert_syncs_into_target(mut ctx: djogi::DjogiContext) {
    // 1. Target has "alpha" (v1). Source has "alpha" (v2) and "beta" (new).
    seed_target(&mut ctx, "alpha", "v1").await;
    seed_source(&mut ctx, "alpha", "v2").await;
    seed_source(&mut ctx, "beta", "new").await;

    let counts = MergeSource::objects()
        .merge_into::<MergeTarget, _, _>(|target, source| {
            target.external_id().merge_on_eq(source.external_id())
        })
        .when_matched_and_update(
            None::<MergeWhenCondition<MergeSource, MergeTarget>>,
            vec![
                MergeTarget::fields()
                    .payload()
                    .merge_copy_from(MergeSource::fields().payload()),
            ],
        )
        .when_not_matched_then_insert(
            None::<MergeWhenCondition<MergeSource, MergeTarget>>,
            vec![
                MergeTarget::fields()
                    .external_id()
                    .merge_insert_from(MergeSource::fields().external_id()),
                MergeTarget::fields()
                    .payload()
                    .merge_insert_from(MergeSource::fields().payload()),
                MergeTarget::fields().active().merge_insert_value(true),
            ],
        )
        .execute(&mut ctx)
        .await
        .map_err(|e| {
            eprintln!("MERGE EXECUTE ERROR: {:?}", e);
            e
        })
        .unwrap();

    // 1 matched (alpha) + 1 not matched (beta) = 2 affected rows.
    assert_eq!(counts.total_affected, 2);

    // Verify "alpha" was updated
    let alpha = MergeTarget::objects()
        .filter(|f| f.external_id().eq("alpha".to_string()))
        .fetch_one(&mut ctx)
        .await
        .unwrap();
    assert_eq!(alpha.payload, "v2");

    // Verify "beta" was inserted
    let beta = MergeTarget::objects()
        .filter(|f| f.external_id().eq("beta".to_string()))
        .fetch_one(&mut ctx)
        .await
        .unwrap();
    assert_eq!(beta.payload, "new");
}

// ── NOT MATCHED BY SOURCE ────────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [MergeSource, MergeTarget])]
async fn merge_delete_missing_source_rows(mut ctx: djogi::DjogiContext) {
    // Target has "alpha" and "gamma". Source only has "alpha".
    // "gamma" should be deleted (or soft-deleted).
    seed_target(&mut ctx, "alpha", "v1").await;
    seed_target(&mut ctx, "gamma", "v1").await;
    seed_source(&mut ctx, "alpha", "v1").await;

    let counts = MergeSource::objects()
        .merge_into::<MergeTarget, _, _>(|target, source| {
            target.external_id().merge_on_eq(source.external_id())
        })
        .when_not_matched_by_source_then_delete(
            None::<MergeWhenCondition<MergeSource, MergeTarget>>,
        )
        .execute(&mut ctx)
        .await
        .map_err(|e| {
            eprintln!("MERGE EXECUTE ERROR: {:?}", e);
            e
        })
        .unwrap();

    // "alpha" matches (no action), "gamma" doesn't match source -> deleted.
    assert_eq!(counts.total_affected, 1);

    let exists = MergeTarget::objects()
        .filter(|f| f.external_id().eq("gamma".to_string()))
        .exists(&mut ctx)
        .await
        .unwrap();
    assert!(!exists, "gamma should have been deleted");

    let alpha_exists = MergeTarget::objects()
        .filter(|f| f.external_id().eq("alpha".to_string()))
        .exists(&mut ctx)
        .await
        .unwrap();
    assert!(alpha_exists, "alpha should still exist");
}

#[djogi::djogi_test(sync_models = [MergeSource, MergeTarget])]
async fn merge_by_source_condition_scopes_target_rows(mut ctx: djogi::DjogiContext) {
    seed_target(&mut ctx, "alpha", "v1").await;
    seed_target(&mut ctx, "gamma", "v1").await;
    seed_target(&mut ctx, "delta", "v1").await;
    seed_source(&mut ctx, "alpha", "v1").await;

    let counts = MergeSource::objects()
        .merge_into::<MergeTarget, _, _>(|target, source| {
            target.external_id().merge_on_eq(source.external_id())
        })
        .when_not_matched_by_source_then_update(
            Some(
                MergeTarget::fields()
                    .external_id()
                    .merge_target_eq_value::<MergeSource, _>("gamma"),
            ),
            MergeTarget::fields().active().merge_set(false),
        )
        .execute(&mut ctx)
        .await
        .unwrap();

    assert_eq!(counts.total_affected, 1);

    let gamma = MergeTarget::objects()
        .filter(|f| f.external_id().eq("gamma"))
        .fetch_one(&mut ctx)
        .await
        .unwrap();
    let delta = MergeTarget::objects()
        .filter(|f| f.external_id().eq("delta"))
        .fetch_one(&mut ctx)
        .await
        .unwrap();

    assert!(
        !gamma.active,
        "gamma should be scoped by the BY SOURCE condition"
    );
    assert!(
        delta.active,
        "delta should not be affected by the scoped condition"
    );
}

#[djogi::djogi_test(sync_models = [MergeSource, MergeTarget])]
async fn merge_not_matched_condition_scopes_source_rows(mut ctx: djogi::DjogiContext) {
    seed_source(&mut ctx, "beta", "new").await;
    seed_source(&mut ctx, "gamma", "skip").await;

    let counts = MergeSource::objects()
        .merge_into::<MergeTarget, _, _>(|target, source| {
            target.external_id().merge_on_eq(source.external_id())
        })
        .when_not_matched_then_insert(
            Some(
                MergeSource::fields()
                    .payload()
                    .merge_source_eq_value::<MergeTarget, _>("new"),
            ),
            vec![
                MergeTarget::fields()
                    .external_id()
                    .merge_insert_from(MergeSource::fields().external_id()),
                MergeTarget::fields()
                    .payload()
                    .merge_insert_from(MergeSource::fields().payload()),
                MergeTarget::fields().active().merge_insert_value(true),
            ],
        )
        .execute(&mut ctx)
        .await
        .unwrap();

    assert_eq!(counts.total_affected, 1);
    assert!(
        MergeTarget::objects()
            .filter(|f| f.external_id().eq("beta"))
            .exists(&mut ctx)
            .await
            .unwrap()
    );
    assert!(
        !MergeTarget::objects()
            .filter(|f| f.external_id().eq("gamma"))
            .exists(&mut ctx)
            .await
            .unwrap()
    );
}

// ── Update if Changed ────────────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [MergeSource, MergeTarget])]
async fn merge_update_changed_only_prevents_unnecessary_stamps(mut ctx: djogi::DjogiContext) {
    // Target and Source both have "alpha" with "v1".
    // when_matched_update_changed should result in 0 affected rows.
    let target = seed_target(&mut ctx, "alpha", "v1").await;
    seed_source(&mut ctx, "alpha", "v1").await;

    // Small sleep to ensure updated_at comparison is reliable if it were to change.
    sleep(Duration::from_millis(10)).await;

    let counts = MergeSource::objects()
        .merge_into::<MergeTarget, _, _>(|target, source| {
            target.external_id().merge_on_eq(source.external_id())
        })
        .when_matched_update_changed(vec![
            MergeTarget::fields()
                .payload()
                .merge_copy_from(MergeSource::fields().payload()),
        ])
        .execute(&mut ctx)
        .await
        .map_err(|e| {
            eprintln!("MERGE EXECUTE ERROR: {:?}", e);
            e
        })
        .unwrap();

    assert_eq!(
        counts.total_affected, 0,
        "no rows should be affected if payload is identical"
    );

    let final_row = MergeTarget::get(&mut ctx, target.id).await.unwrap();
    assert_eq!(
        final_row.updated_at, target.updated_at,
        "updated_at should not have advanced"
    );
}

// ── Validation Rejections ────────────────────────────────────────────────

#[djogi::djogi_test(sync_models = [MergeSource, MergeTarget])]
async fn merge_rejects_structural_none_source_with_by_source_branch(mut ctx: djogi::DjogiContext) {
    let res = MergeSource::objects()
        .none()
        .merge_into::<MergeTarget, _, _>(|target, source| {
            target.external_id().merge_on_eq(source.external_id())
        })
        .when_not_matched_by_source_then_delete(
            None::<MergeWhenCondition<MergeSource, MergeTarget>>,
        )
        .execute(&mut ctx)
        .await;

    assert!(res.is_err());
    let err = res.unwrap_err();
    assert!(format!("{}", err).contains("structural-empty source (.none()) is rejected"));
}

#[djogi::djogi_test(sync_models = [MergeSource, MergeTarget])]
async fn merge_rejects_by_source_predicate_referencing_source(mut ctx: djogi::DjogiContext) {
    let res = MergeSource::objects()
        .merge_into::<MergeTarget, _, _>(|target, source| {
            target.external_id().merge_on_eq(source.external_id())
        })
        .when_not_matched_by_source_then_delete(Some(
            MergeTarget::fields()
                .payload()
                .is_distinct_from_source(MergeSource::fields().payload()),
        ))
        .execute(&mut ctx)
        .await;

    assert!(res.is_err());
    let err = res.unwrap_err();
    assert!(
        format!("{}", err).contains("BY SOURCE branch condition cannot reference source field")
    );
}
