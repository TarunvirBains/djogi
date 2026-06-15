// Issue #442 — typed CTE query builder: SQL shape pins.
//
// `QuerySet::with` / `.with_recursive(...)` build a `CteQuerySet<T>` whose
// terminal calls emit `WITH [RECURSIVE] <name> AS (<body>)[ CYCLE ...]
// SELECT <cols> FROM <from_cte> <tail>`. This fixture pins the non-recursive
// and recursive SQL shape through the public adopter surface only.

use djogi::prelude::*;

#[model(table = "c442_cte_nodes", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Node {
    pub label: String,
    pub parent_id: i64,
    pub active: bool,
}

#[test]
fn non_recursive_cte_shape() {
    let cte = Node::objects()
        .with("recent", Node::objects().filter(|f| f.active().eq(true)))
        .expect("with")
        .from_cte("recent")
        .expect("from_cte");
    let sql = cte.__sql_for_test().expect("sql");
    assert!(sql.starts_with("WITH recent AS ("), "{sql}");
    assert!(!sql.contains("WITH RECURSIVE"), "{sql}");
    assert!(sql.contains("FROM recent"), "{sql}");
    assert!(sql.contains("active = $1"), "{sql}");
}

#[test]
fn non_recursive_cte_renumbers_body_and_consumer_binds() {
    let cte = Node::objects()
        .with("recent", Node::objects().filter(|f| f.active().eq(true)))
        .expect("with")
        .from_cte("recent")
        .expect("from_cte")
        .filter(|f| f.parent_id().eq(7_i64));
    let sql = cte.__sql_for_test().expect("sql");
    assert!(sql.contains("active = $1"), "{sql}");
    assert!(sql.contains("FROM recent WHERE parent_id = $2"), "{sql}");
}

#[test]
fn recursive_cte_with_cycle_shape() {
    let anchor = Node::objects().filter(|f| f.parent_id().eq(0_i64));
    let arm = RecursiveArm::<Node>::referencing("walk")
        .join_on("parent_id", "id")
        .expect("join_on");
    let cte = Node::objects()
        .with_recursive("walk", anchor, arm)
        .expect("with_recursive")
        .cycle(&["id"], "is_cycle", "cycle_path")
        .expect("cycle")
        .from_cte("walk")
        .expect("from_cte")
        .exclude_cycle_rows()
        .expect("exclude_cycle_rows");
    let sql = cte.__sql_for_test().expect("sql");
    assert!(sql.starts_with("WITH RECURSIVE walk AS ("), "{sql}");
    assert!(
        sql.contains("FROM c442_cte_nodes t JOIN walk cte ON t.parent_id = cte.id"),
        "{sql}"
    );
    assert!(sql.contains(") CYCLE id SET is_cycle USING cycle_path"), "{sql}");
    assert!(sql.contains("FROM walk WHERE NOT is_cycle"), "{sql}");
}

#[test]
fn invalid_cte_name_rejected_at_with() {
    let err = Node::objects()
        .with("__djogi_x", Node::objects())
        .expect_err("reserved prefix rejected");
    assert!(matches!(err, djogi::DjogiError::Validation(_)), "{err:?}");
}
