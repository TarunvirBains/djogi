use djogi::prelude::*;

#[model(table = "phase8_5_c4b_lateral_live_projects", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Project {
    pub name: String,
}

#[model(table = "phase8_5_c4b_lateral_live_tasks", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Task {
    pub project_id: HeerId,
    pub name: String,
    pub priority: i32,
}

#[djogi::djogi_test(sync_models = [Project, Task])]
async fn test_lateral_join_live(mut ctx: djogi::DjogiContext) {
    let p1 = Project::create(
        &mut ctx,
        Project {
            name: "P1".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let p2 = Project::create(
        &mut ctx,
        Project {
            name: "P2".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    Task::create(
        &mut ctx,
        Task {
            project_id: p1.id,
            name: "T1.1".into(),
            priority: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    Task::create(
        &mut ctx,
        Task {
            project_id: p1.id,
            name: "T1.2".into(),
            priority: 2,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    Task::create(
        &mut ctx,
        Task {
            project_id: p2.id,
            name: "T2.1".into(),
            priority: 3,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Inner lateral: Highest priority task per project
    let inner = Task::objects()
        .filter_expr(|f| {
            f.project_id()
                .as_expr()
                .eq(ProjectOuterRef::id().as_lateral_outer_expr())
        })
        .order_by(|f| f.priority().desc())
        .limit(1);

    let rows = Project::objects()
        .join_lateral(inner)
        .fetch_all(&mut ctx)
        .await
        .unwrap();

    assert_eq!(rows.len(), 2);
    // Sort by project name for stable assert
    let mut rows = rows;
    rows.sort_by(|(p_a, _), (p_b, _)| p_a.name.cmp(&p_b.name));

    assert_eq!(rows[0].0.name, "P1");
    assert_eq!(rows[0].1.name, "T1.2");
    assert_eq!(rows[1].0.name, "P2");
    assert_eq!(rows[1].1.name, "T2.1");
}

#[djogi::djogi_test(sync_models = [Project, Task])]
async fn test_left_lateral_join_live(mut ctx: djogi::DjogiContext) {
    let p1 = Project::create(
        &mut ctx,
        Project {
            name: "P1".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let _p2 = Project::create(
        &mut ctx,
        Project {
            name: "P2".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    Task::create(
        &mut ctx,
        Task {
            project_id: p1.id,
            name: "T1.1".into(),
            priority: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // p2 has no tasks.
    let inner = Task::objects()
        .filter_expr(|f| {
            f.project_id()
                .as_expr()
                .eq(ProjectOuterRef::id().as_lateral_outer_expr())
        })
        .limit(1);

    let rows = Project::objects()
        .left_join_lateral(inner)
        .fetch_all(&mut ctx)
        .await
        .unwrap();

    assert_eq!(rows.len(), 2);
    let mut rows = rows;
    rows.sort_by(|(p_a, _), (p_b, _)| p_a.name.cmp(&p_b.name));

    assert_eq!(rows[0].0.name, "P1");
    assert!(rows[0].1.is_some());
    assert_eq!(rows[1].0.name, "P2");
    assert!(rows[1].1.is_none());
}

#[djogi::djogi_test(sync_models = [Project, Task])]
async fn test_lateral_count_live(mut ctx: djogi::DjogiContext) {
    let p1 = Project::create(
        &mut ctx,
        Project {
            name: "P1".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let _p2 = Project::create(
        &mut ctx,
        Project {
            name: "P2".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    Task::create(
        &mut ctx,
        Task {
            project_id: p1.id,
            name: "T1.1".into(),
            priority: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let inner = Task::objects()
        .filter_expr(|f| {
            f.project_id()
                .as_expr()
                .eq(ProjectOuterRef::id().as_lateral_outer_expr())
        })
        .limit(1);

    let inner_count = Project::objects()
        .join_lateral(inner.clone())
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(inner_count, 1); // Only P1 has a task

    let left_count = Project::objects()
        .left_join_lateral(inner)
        .count(&mut ctx)
        .await
        .unwrap();
    assert_eq!(left_count, 2); // Both P1 and P2 appear (P2 with None right side)
}
