// Issue #442 — typed CTE query builder: live Postgres tests.

use djogi::prelude::*;

#[model(table = "c442_cte_live_nodes", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Node {
    pub label: String,
    pub active: bool,
}

async fn seed_nodes(ctx: &mut djogi::DjogiContext) {
    for (label, active) in [
        ("alpha", true),
        ("beta", false),
        ("gamma", true),
        ("delta", true),
    ] {
        Node::create(
            ctx,
            Node {
                label: label.to_string(),
                active,
                ..Default::default()
            },
        )
        .await
        .expect("seed node");
    }
}

#[djogi::djogi_test(sync_models = [Node])]
async fn non_recursive_cte_returns_filtered_rows(mut ctx: djogi::DjogiContext) {
    seed_nodes(&mut ctx).await;

    let active_nodes = Node::objects().filter(|f| f.active().eq(true));
    let rows = Node::objects()
        .with("active_nodes", active_nodes)
        .expect("with")
        .from_cte("active_nodes")
        .expect("from_cte")
        .order_by(|f| f.label().asc())
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all");

    let labels: Vec<String> = rows.into_iter().map(|n| n.label).collect();
    assert_eq!(labels, vec!["alpha", "delta", "gamma"]);
}

#[djogi::djogi_test(sync_models = [Node])]
async fn non_recursive_cte_count_matches(mut ctx: djogi::DjogiContext) {
    seed_nodes(&mut ctx).await;

    let active_nodes = Node::objects().filter(|f| f.active().eq(true));
    let n = Node::objects()
        .with("active_nodes", active_nodes)
        .expect("with")
        .from_cte("active_nodes")
        .expect("from_cte")
        .count(&mut ctx)
        .await
        .expect("count");

    assert_eq!(n, 3);
}

#[djogi::djogi_test(sync_models = [Node])]
async fn non_recursive_cte_exists_and_first_match(mut ctx: djogi::DjogiContext) {
    seed_nodes(&mut ctx).await;

    let active_nodes = Node::objects().filter(|f| f.active().eq(true));
    let exists = Node::objects()
        .with("active_nodes", active_nodes.clone())
        .expect("with")
        .from_cte("active_nodes")
        .expect("from_cte")
        .exists(&mut ctx)
        .await
        .expect("exists");
    assert!(exists);

    let first = Node::objects()
        .with("active_nodes", active_nodes)
        .expect("with")
        .from_cte("active_nodes")
        .expect("from_cte")
        .order_by(|f| f.label().asc())
        .first(&mut ctx)
        .await
        .expect("first")
        .expect("first active row");
    assert_eq!(first.label, "alpha");
}

#[model(table = "c442_cte_live_tree", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub name: String,
    pub parent_id: Option<i64>,
}

async fn seed_chain(ctx: &mut djogi::DjogiContext) -> TreeNode {
    let root = TreeNode::create(
        ctx,
        TreeNode {
            name: "root".into(),
            parent_id: None,
            ..Default::default()
        },
    )
    .await
    .expect("seed root");
    let mid = TreeNode::create(
        ctx,
        TreeNode {
            name: "mid".into(),
            parent_id: Some(root.id.as_i64()),
            ..Default::default()
        },
    )
    .await
    .expect("seed mid");
    TreeNode::create(
        ctx,
        TreeNode {
            name: "leaf".into(),
            parent_id: Some(mid.id.as_i64()),
            ..Default::default()
        },
    )
    .await
    .expect("seed leaf")
}

#[djogi::djogi_test(sync_models = [TreeNode])]
async fn recursive_cte_walks_ancestors(mut ctx: djogi::DjogiContext) {
    let leaf = seed_chain(&mut ctx).await;

    let anchor = TreeNode::objects().filter(|f| f.id().eq(leaf.id));
    let up = RecursiveArm::<TreeNode>::referencing("ancestors")
        .join_on("id", "parent_id")
        .expect("join_on");

    let rows = TreeNode::objects()
        .with_recursive("ancestors", anchor, up)
        .expect("with_recursive")
        .from_cte("ancestors")
        .expect("from_cte")
        .order_by(|f| f.name().asc())
        .fetch_all(&mut ctx)
        .await
        .expect("fetch_all");

    let names: Vec<String> = rows.into_iter().map(|n| n.name).collect();
    assert_eq!(names, vec!["leaf", "mid", "root"]);
}

#[djogi::djogi_test(sync_models = [TreeNode])]
async fn recursive_cte_with_cycle_terminates(mut ctx: djogi::DjogiContext) {
    let leaf = seed_chain(&mut ctx).await;

    let mut root = TreeNode::objects()
        .filter(|f| f.parent_id().is_null())
        .first(&mut ctx)
        .await
        .expect("query root")
        .expect("root exists");
    root.parent_id = Some(leaf.id.as_i64());
    root.save(&mut ctx).await.expect("close cycle");

    let anchor = TreeNode::objects().filter(|f| f.id().eq(leaf.id));
    let up = RecursiveArm::<TreeNode>::referencing("ancestors")
        .join_on("id", "parent_id")
        .expect("join_on");

    let rows = TreeNode::objects()
        .with_recursive("ancestors", anchor, up)
        .expect("with_recursive")
        .cycle(&["id"], "is_cycle", "cycle_path")
        .expect("cycle")
        .from_cte("ancestors")
        .expect("from_cte")
        .exclude_cycle_rows()
        .expect("exclude_cycle_rows")
        .fetch_all(&mut ctx)
        .await
        .expect("cycle walk");

    assert_eq!(rows.len(), 3);
}
