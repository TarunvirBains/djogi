// djogi#485 — correlated single-table scalar subquery embedded in an
// INSERT ... SELECT projection column, against a live Postgres.
//
// Shape under test (single-COLUMN correlated scalar subquery — the shape
// `InsertSelectSource::subquery` can express today):
//
//   INSERT INTO summaries (parent_id, score)
//   SELECT parents.id,
//          (SELECT profiles.score FROM profiles
//           WHERE profiles.parent_id = parents.id)
//   FROM parents
//
// The correlation is 1:1 (each parent has exactly one profile), so the
// scalar subquery returns exactly one row — required, because `Subquery`
// drops ordering/limit, so a multi-row correlated scalar would raise "more
// than one row returned by a subquery used as an expression" at runtime. The
// subquery references the outer `parents` row via the qualified outer-ref
// form (`parents.id`), which resolves because INSERT...SELECT emits
// `FROM parents` (the bare physical table name).
//
// An aggregate projection (`SELECT COUNT(*) FROM ...`) as the subquery scalar
// is a separate surface — `Subquery` projects a column, not an aggregate.

use djogi::prelude::*;

#[model(table = "isc_parents", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct IscParent {
    pub label: String,
}

// One profile per parent (1:1) carrying the scalar the subquery projects.
#[model(table = "isc_profiles", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct IscProfile {
    pub parent_id: djogi::HeerIdDesc,
    pub score: i64,
}

#[model(table = "isc_summaries", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct IscSummary {
    pub parent_id: djogi::HeerIdDesc,
    pub score: i64,
}

#[djogi::djogi_test(sync_models = [IscParent, IscProfile, IscSummary])]
async fn correlated_subquery_in_insert_select_projects_profile_score(mut ctx: djogi::DjogiContext) {
    // Seed: two parents, each with exactly one profile carrying a score.
    let p1 = IscParent::create(
        &mut ctx,
        IscParent {
            label: "p1".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create p1");
    let p2 = IscParent::create(
        &mut ctx,
        IscParent {
            label: "p2".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create p2");
    IscProfile::create(
        &mut ctx,
        IscProfile {
            parent_id: *p1.pk_value(),
            score: 42,
            ..Default::default()
        },
    )
    .await
    .expect("create profile of p1");
    IscProfile::create(
        &mut ctx,
        IscProfile {
            parent_id: *p2.pk_value(),
            score: 7,
            ..Default::default()
        },
    )
    .await
    .expect("create profile of p2");

    // Build the correlated single-column subquery: profiles.score WHERE
    // profiles.parent_id = <outer parent>.id. Then INSERT INTO summaries
    // (parent_id, score) SELECT parents.id, <subquery> FROM parents.
    IscParent::objects()
        .insert_into::<IscSummary, _, _>(|target, source| {
            // Correlated profile-score subquery for the current outer parent row.
            // `Subquery::new(qs, column)` takes a column selector — the
            // `IscProfile` field accessor `score()` (an `IntoSqlField<IscProfile, i64>`),
            // so the projected scalar is `profiles.score`.
            let profile_score = djogi::expr::subquery::Subquery::new(
                IscProfile::objects().filter_expr(|c| {
                    c.parent_id()
                        .as_expr()
                        .eq(IscParentOuterRef::id().as_qualified_expr())
                }),
                <IscProfile as djogi::model::Model>::Fields::default().score(),
            );
            vec![
                target.parent_id().copy_from(source.id().as_insert_source()),
                target
                    .score()
                    .copy_from(InsertSelectSource::subquery(profile_score)),
            ]
        })
        .execute(&mut ctx)
        .await
        .expect("correlated insert-select");

    // Verify: each summary carries the correlated profile's score.
    let summaries = IscSummary::objects()
        .order_by(|s| s.parent_id().asc())
        .fetch_all(&mut ctx)
        .await
        .expect("fetch summaries");
    let p1_sum = summaries
        .iter()
        .find(|s| s.parent_id == *p1.pk_value())
        .expect("p1 summary");
    assert_eq!(p1_sum.score, 42, "p1 summary should carry profile score 42");
    let p2_sum = summaries
        .iter()
        .find(|s| s.parent_id == *p2.pk_value())
        .expect("p2 summary");
    assert_eq!(p2_sum.score, 7, "p2 summary should carry profile score 7");
}
