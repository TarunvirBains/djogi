use djogi::prelude::*;

#[model(table = "lateral_live_projects", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Project {
    pub name: String,
}

#[model(table = "lateral_live_tasks", pk = HeerId)]
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
            name: ".1".into(),
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
            name: ".2".into(),
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
            name: ".1".into(),
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
    assert_eq!(rows[0].1.name, ".2");
    assert_eq!(rows[1].0.name, "P2");
    assert_eq!(rows[1].1.name, ".1");
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
    let _project2 = Project::create(
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
            name: ".1".into(),
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
async fn test_left_lateral_join_none_keeps_outer_rows(mut ctx: djogi::DjogiContext) {
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
            name: ".1".into(),
            priority: 1,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    Task::create(
        &mut ctx,
        Task {
            project_id: p2.id,
            name: ".1".into(),
            priority: 2,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let inner = Task::objects().none();

    let rows = Project::objects()
        .left_join_lateral(inner.clone())
        .fetch_all(&mut ctx)
        .await
        .unwrap();

    assert_eq!(rows.len(), 2);
    let mut rows = rows;
    rows.sort_by(|(p_a, _), (p_b, _)| p_a.name.cmp(&p_b.name));

    assert_eq!(rows[0].0.name, "P1");
    assert!(rows[0].1.is_none());
    assert_eq!(rows[1].0.name, "P2");
    assert!(rows[1].1.is_none());

    let count = Project::objects()
        .left_join_lateral(inner)
        .count(&mut ctx)
        .await
        .unwrap();

    assert_eq!(count, 2);
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
    let _project2 = Project::create(
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
            name: ".1".into(),
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

#[djogi::djogi_test(sync_models = [Project, Task])]
async fn test_outer_limit_applies_before_lateral_fan_out(mut ctx: djogi::DjogiContext) {
    let p1 = Project::create(
        &mut ctx,
        Project {
            name: "A".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let _project2 = Project::create(
        &mut ctx,
        Project {
            name: "B".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    Task::create(
        &mut ctx,
        Task {
            project_id: p1.id,
            name: "A.1".into(),
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
            name: "A.2".into(),
            priority: 2,
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
        .order_by(|f| f.priority().desc());

    let rows = Project::objects()
        .order_by(|f| f.name().asc())
        .limit(1)
        .join_lateral(inner)
        .fetch_all(&mut ctx)
        .await
        .unwrap();

    assert_eq!(
        rows.len(),
        2,
        "outer limit must select one outer row before fan-out"
    );
    assert!(rows.iter().all(|(p, _)| p.id == p1.id));
}

#[djogi::djogi_test(sync_models = [Project, Task])]
async fn test_outer_distinct_on_applies_before_lateral_fan_out(mut ctx: djogi::DjogiContext) {
    let p1 = Project::create(
        &mut ctx,
        Project {
            name: "Dup".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let p2 = Project::create(
        &mut ctx,
        Project {
            name: "Dup".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let p3 = Project::create(
        &mut ctx,
        Project {
            name: "Solo".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    for (project_id, task_name) in [(p1.id, "D1"), (p2.id, "D2"), (p3.id, "S1")] {
        Task::create(
            &mut ctx,
            Task {
                project_id,
                name: task_name.into(),
                priority: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    let inner = Task::objects()
        .filter_expr(|f| {
            f.project_id()
                .as_expr()
                .eq(ProjectOuterRef::id().as_lateral_outer_expr())
        })
        .limit(1);

    let rows = Project::objects()
        .distinct_on(|f| f.name())
        .order_by(|f| f.name().asc())
        .order_by(|f| f.id().asc())
        .join_lateral(inner)
        .fetch_all(&mut ctx)
        .await
        .unwrap();

    assert_eq!(
        rows.len(),
        2,
        "distinct_on(name) must dedupe outer rows before fan-out"
    );
    assert_eq!(rows.iter().filter(|(p, _)| p.name == "Dup").count(), 1);
    assert_eq!(rows.iter().filter(|(p, _)| p.name == "Solo").count(), 1);
}

#[djogi::djogi_test(sync_models = [Project, Task])]
async fn test_lateral_first_is_tuple_level_limit(mut ctx: djogi::DjogiContext) {
    let p1 = Project::create(
        &mut ctx,
        Project {
            name: "Only".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    Task::create(
        &mut ctx,
        Task {
            project_id: p1.id,
            name: "high".into(),
            priority: 10,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    Task::create(
        &mut ctx,
        Task {
            project_id: p1.id,
            name: "low".into(),
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
        .order_by(|f| f.priority().desc());

    let row = Project::objects()
        .join_lateral(inner)
        .first(&mut ctx)
        .await
        .unwrap()
        .expect("first() must return one tuple");

    assert_eq!(row.1.name, "high");
}

#[djogi::djogi_test(sync_models = [Project, Task])]
async fn test_lateral_validation_runs_before_empty_short_circuit(mut ctx: djogi::DjogiContext) {
    let inner_err = Project::objects()
        .join_lateral(Task::objects().none().select_for_update())
        .count(&mut ctx)
        .await
        .unwrap_err();
    assert!(
        matches!(inner_err, djogi::DjogiError::Validation(ref msg) if msg.contains("row locks")),
        "expected validation error for empty inner + lock, got {inner_err:?}"
    );

    let outer_err = Project::objects()
        .none()
        .select_for_update()
        .left_join_lateral(Task::objects().limit(1))
        .fetch_all(&mut ctx)
        .await
        .unwrap_err();
    assert!(
        matches!(outer_err, djogi::DjogiError::Validation(ref msg) if msg.contains("row locks")),
        "expected validation error for empty outer + lock, got {outer_err:?}"
    );
}
