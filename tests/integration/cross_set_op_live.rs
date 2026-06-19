// Issue #462 — cross-model set operations: live Postgres tests.
#![allow(dead_code)]

use djogi::prelude::*;

#[model(table = "x462_live_logins", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Login {
    pub actor: String,
    pub occurred: i32,
}

#[model(table = "x462_live_edits", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Edit {
    pub actor: String,
    pub occurred: i32,
}

#[model(table = "x462_live_activity", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Activity {
    pub actor: String,
    pub occurred: i32,
}

async fn seed(ctx: &mut djogi::DjogiContext) {
    for (actor, occurred) in [("ann", 10), ("bob", 20), ("cat", 30)] {
        Login::create(
            ctx,
            Login {
                actor: actor.to_string(),
                occurred,
                ..Default::default()
            },
        )
        .await
        .expect("seed Login");
    }
    for (actor, occurred) in [("ann", 40), ("dan", 50)] {
        Edit::create(
            ctx,
            Edit {
                actor: actor.to_string(),
                occurred,
                ..Default::default()
            },
        )
        .await
        .expect("seed Edit");
    }
}

#[djogi::djogi_test(sync_models = [Login, Edit, Activity])]
async fn cross_union_all_merges_two_models(mut ctx: djogi::DjogiContext) {
    seed(&mut ctx).await;
    let rows: Vec<Activity> =
        djogi::query::union_all_as::<Activity, _, _>(Login::objects(), Edit::objects())
            .fetch_all(&mut ctx)
            .await
            .unwrap();
    assert_eq!(rows.len(), 5, "3 logins + 2 edits = 5");

    let mut actors: Vec<String> = rows.iter().map(|a| a.actor.clone()).collect();
    actors.sort();
    assert_eq!(actors, vec!["ann", "ann", "bob", "cat", "dan"]);
}

#[djogi::djogi_test(sync_models = [Login, Edit, Activity])]
async fn cross_union_all_outer_order_and_limit_apply(mut ctx: djogi::DjogiContext) {
    seed(&mut ctx).await;
    let rows: Vec<Activity> =
        djogi::query::union_all_as::<Activity, _, _>(Login::objects(), Edit::objects())
            .order_by("occurred", OuterOrder::Desc)
            .limit(2)
            .fetch_all(&mut ctx)
            .await
            .unwrap();
    let occ: Vec<i32> = rows.iter().map(|a| a.occurred).collect();
    assert_eq!(occ, vec![50, 40]);
}

#[djogi::djogi_test(sync_models = [Login, Edit, Activity])]
async fn cross_count_reports_combined_cardinality(mut ctx: djogi::DjogiContext) {
    seed(&mut ctx).await;
    let n = djogi::query::union_all_as::<Activity, _, _>(Login::objects(), Edit::objects())
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 5);
}

#[djogi::djogi_test(sync_models = [Login, Edit, Activity])]
async fn cross_first_returns_outer_ordered_row(mut ctx: djogi::DjogiContext) {
    seed(&mut ctx).await;
    let row: Option<Activity> =
        djogi::query::union_all_as::<Activity, _, _>(Login::objects(), Edit::objects())
            .order_by("occurred", OuterOrder::Asc)
            .first(&mut ctx)
            .await
            .unwrap();
    let row = row.expect("non-empty merged set");
    assert_eq!(row.occurred, 10);
    assert_eq!(row.actor, "ann");
}

#[djogi::djogi_test(sync_models = [Login, Edit, Activity])]
async fn cross_intersect_returns_rows_in_both(mut ctx: djogi::DjogiContext) {
    seed(&mut ctx).await;
    // Full-projection intersect of disjoint-id tables is empty.
    let n = djogi::query::intersect_as::<Activity, _, _>(Login::objects(), Edit::objects())
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 0, "disjoint id tables → empty intersect");
}

#[djogi::djogi_test(sync_models = [Login, Edit, Activity])]
async fn cross_except_subtracts_right_from_left(mut ctx: djogi::DjogiContext) {
    seed(&mut ctx).await;
    // No left row matches a right row → all left remain.
    let n = djogi::query::except_as::<Activity, _, _>(Login::objects(), Edit::objects())
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(n, 3, "all 3 left rows remain");
}

#[djogi::djogi_test(sync_models = [Login, Edit, Activity])]
async fn cross_arm_with_lock_rejected_at_terminal(mut ctx: djogi::DjogiContext) {
    seed(&mut ctx).await;
    let plain = Login::objects();
    let locked = Edit::objects().select_for_update();
    let err = djogi::query::union_as::<Activity, _, _>(plain, locked)
        .fetch_all(&mut ctx)
        .await
        .unwrap_err();
    assert!(
        matches!(err, DjogiError::SetOpArmInvalid { side, .. } if side == "right"),
        "lock on right arm: {err:?}"
    );
    assert!(err.is_terminal(), "SetOpArmInvalid is terminal");
}

// ── Tenant wiring regression ──

#[model(table = "x462_live_tlogins", pk = HeerId, tenant_key = "org_id")]
#[derive(Debug, Clone)]
pub struct TLogin {
    pub org_id: String,
    pub actor: String,
}

#[model(table = "x462_live_tedits", pk = HeerId, tenant_key = "org_id")]
#[derive(Debug, Clone)]
pub struct TEdit {
    pub org_id: String,
    pub actor: String,
}

#[model(table = "x462_live_tactivity", pk = HeerId, tenant_key = "org_id")]
#[derive(Debug, Clone)]
pub struct TActivity {
    pub org_id: String,
    pub actor: String,
}

#[djogi::djogi_test(sync_models = [TLogin, TEdit, TActivity])]
async fn cross_tenant_wiring_sets_guc_and_does_not_poison(mut ctx: djogi::DjogiContext) {
    use djogi::auth::AuthContext;

    ctx.set_auth(AuthContext::new(djogi::HeerId::from_i64(1).unwrap()).with_tenant("org_a"));

    // Must run inside transaction for tenant GUC to be observable.
    djogi::transaction::atomic(&mut ctx, |ctx| {
        Box::pin(async move {
            for actor in ["ann", "bob"] {
                TLogin::create(
                    ctx,
                    TLogin {
                        org_id: "org_a".to_string(),
                        actor: actor.to_string(),
                        ..Default::default()
                    },
                )
                .await
                .expect("seed TLogin");
            }
            TEdit::create(
                ctx,
                TEdit {
                    org_id: "org_a".to_string(),
                    actor: "cat".to_string(),
                    ..Default::default()
                },
            )
            .await
            .expect("seed TEdit");

            let rows: Vec<TActivity> =
                djogi::query::union_all_as::<TActivity, _, _>(TLogin::objects(), TEdit::objects())
                    .fetch_all(ctx)
                    .await
                    .unwrap();
            assert_eq!(rows.len(), 3, "2 tlogins + 1 tedit");

            let mut actors: Vec<String> = rows.iter().map(|r| r.actor.clone()).collect();
            actors.sort();
            assert_eq!(actors, vec!["ann", "bob", "cat"]);

            Ok(())
        })
    })
    .await
    .unwrap();
}

#[djogi::djogi_test(sync_models = [Login, Edit, Activity])]
async fn cross_filtered_arms_merge_correctly(mut ctx: djogi::DjogiContext) {
    seed(&mut ctx).await;

    // The motivating example: recent logins (occurred >= 20) merged with
    // recent edits (occurred >= 40).
    let recent_logins = Login::objects().filter(|f| f.occurred().gte(20i32));
    let recent_edits = Edit::objects().filter(|f| f.occurred().gte(40i32));

    let rows: Vec<Activity> =
        djogi::query::union_all_as::<Activity, _, _>(recent_logins, recent_edits)
            .fetch_all(&mut ctx)
            .await
            .unwrap();
    let mut actors: Vec<String> = rows.iter().map(|a| a.actor.clone()).collect();
    actors.sort();
    // logins occurred>=20: bob(20), cat(30) → {bob, cat}
    // edits occurred>=40: ann(40), dan(50) → {ann, dan}
    assert_eq!(actors, vec!["ann", "bob", "cat", "dan"]);
}

// ── Option F acceptance: a cross-SCHEMA merge of two DIFFERENT source models
// via their narrowed visages, decoded as one visage. This is the test that
// proves the feature closes the typed-surface gap for the Message+Reaction
// activity-feed case. MsgEvent and RxnEvent have DIFFERENT non-exposed columns
// (`body` vs `emoji`) but a COMMON exposed shape (actor, occurred); their
// `public` visages therefore project the same column shape and can be unioned.
#[model(table = "x462_live_msg_events", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct MsgEvent {
    #[field(expose(public))]
    pub actor: String,
    #[field(expose(public))]
    pub occurred: i32,
    pub body: String,
}

#[model(table = "x462_live_rxn_events", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct RxnEvent {
    #[field(expose(public))]
    pub actor: String,
    #[field(expose(public))]
    pub occurred: i32,
    pub emoji: String,
}

#[djogi::djogi_test(sync_models = [MsgEvent, RxnEvent])]
async fn cross_schema_union_via_visage_arms_merges_and_decodes(mut ctx: djogi::DjogiContext) {
    // Seed two DIFFERENT-schema source models through the typed CRUD surface.
    for (actor, occurred, body) in [("ann", 10, "hi"), ("bob", 20, "yo")] {
        MsgEvent::create(
            &mut ctx,
            MsgEvent {
                actor: actor.to_string(),
                occurred,
                body: body.to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("seed MsgEvent");
    }
    for (actor, occurred, emoji) in [("ann", 30, "thumbs_up"), ("dan", 40, "fire")] {
        RxnEvent::create(
            &mut ctx,
            RxnEvent {
                actor: actor.to_string(),
                occurred,
                emoji: emoji.to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("seed RxnEvent");
    }

    // Cross-SCHEMA union via the two `public` visages, decoded as
    // MsgEventPublic. The visages project the same shape
    // (id, created_at, updated_at, actor, occurred), so the union aligns and
    // decodes. 2 msgs + 2 rxns = 4 rows.
    let feed: Vec<MsgEventPublic> = djogi::query::union_all_as::<MsgEventPublic, _, _>(
        MsgEventPublic::filter(|f| f.occurred().gte(0i32)),
        RxnEventPublic::filter(|f| f.occurred().gte(0i32)),
    )
    .order_by("occurred", OuterOrder::Asc)
    .fetch_all(&mut ctx)
    .await
    .expect("cross-schema visage union must succeed");

    assert_eq!(
        feed.len(),
        4,
        "2 MsgEvent + 2 RxnEvent merged into the feed"
    );
    let occ: Vec<i32> = feed.iter().map(|v| v.occurred).collect();
    assert_eq!(occ, vec![10, 20, 30, 40], "outer ORDER BY occurred ASC");
    // The decoded visage carries the exposed `actor` value from whichever
    // source row produced it — proving the narrowed projection decoded
    // correctly across two different source schemas.
    let actors: Vec<&str> = feed.iter().map(|v| v.actor.as_str()).collect();
    assert_eq!(actors, vec!["ann", "bob", "ann", "dan"]);
}
