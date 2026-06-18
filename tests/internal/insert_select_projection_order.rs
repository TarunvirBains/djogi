use djogi::prelude::*;

#[model(table = "projection_sources", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct ProjectionSource {
    pub value: i32,
}

#[model(table = "projection_targets", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct ProjectionTarget {
    pub value: i32,
}

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#106): this fixture deliberately recreates the target table in non-canonical column order to prove insert-select RETURNING decodes through Djogi's canonical projection rather than physical table order.
async fn recreate_projection_target_noncanonical_order(ctx: &mut djogi::DjogiContext) {
    ctx.raw_ddl("DROP TABLE IF EXISTS projection_targets")
        .await
        .expect("target projection table drop should succeed");
    ctx.raw_ddl(
        "CREATE TABLE projection_targets (
            id BIGINT PRIMARY KEY DEFAULT generate_id(),
            value INTEGER NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
    )
    .await
    .expect("target projection table should be recreated in non-canonical order");
}

#[djogi::djogi_test(sync_models = [
    ProjectionSource,
    ProjectionTarget
])]
async fn insert_select_execute_returning_uses_canonical_projection_for_noncanonical_ddl(
    mut ctx: djogi::DjogiContext,
) {
    recreate_projection_target_noncanonical_order(&mut ctx).await;

    let source_row = ProjectionSource::create(
        &mut ctx,
        ProjectionSource {
            value: 101,
            ..Default::default()
        },
    )
    .await
    .expect("source row should be created");

    let returned = ProjectionSource::objects()
        .filter(|f| f.id().eq(source_row.id))
        .insert_into::<ProjectionTarget, _, _>(|t, s| {
            vec![t.value().copy_from(s.value().as_insert_source())]
        })
        .execute_returning(&mut ctx)
        .await
        .expect("execute_returning should decode rows via canonical projection");

    assert_eq!(returned.len(), 1, "expected one source row to be copied");
    assert_eq!(
        returned[0].value, 101,
        "copied value should decode correctly"
    );
    assert_ne!(
        returned[0].id, source_row.id,
        "inserted target row should get a fresh framework id"
    );
}
