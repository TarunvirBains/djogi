use djogi::prelude::*;

#[model(table = "phase8_5_c4b_lateral_parents", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Parent {
    pub name: String,
    pub active: bool,
}

#[model(table = "phase8_5_c4b_lateral_children", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Child {
    pub parent_id: HeerId,
    pub score: i64,
}

impl ChildOuterRef {
    // The lateral outer-ref helper is implemented on `OuterRef`.
    // Let's verify it compiles.
}

#[test]
fn test_inner_lateral_shape() {
    let outer = Parent::objects().filter(|f| f.active().eq(true));
    let inner = Child::objects()
        .filter_expr(|f| f.parent_id().as_expr().eq(ParentOuterRef::id().as_lateral_outer_expr()))
        .order_by(|f| f.score().desc())
        .limit(1);

    let lat = outer.join_lateral(inner);
    let sql = lat.__sql_for_test().unwrap();

    assert!(sql.contains("SELECT l.id AS l_id"), "left alias projection: {}", sql);
    assert!(sql.contains("r.id AS r_id"), "right alias projection: {}", sql);
    assert!(sql.contains("FROM phase8_5_c4b_lateral_parents AS l"), "outer from: {}", sql);
    assert!(sql.contains("JOIN LATERAL ("), "inner join lateral: {}", sql);
    assert!(sql.contains("FROM phase8_5_c4b_lateral_children"), "inner from: {}", sql);
    assert!(sql.contains("WHERE parent_id = l.id"), "outer ref correlation: {}", sql);
    assert!(sql.contains("ORDER BY score DESC LIMIT $1"), "inner limit preserved: {}", sql);
    assert!(sql.contains(") AS r ON TRUE"), "on true clause: {}", sql);
    assert!(sql.contains("WHERE l.active = $2"), "outer filter applied: {}", sql);
}

#[test]
fn test_left_lateral_shape() {
    let outer = Parent::objects();
    let inner = Child::objects()
        .filter_expr(|f| f.parent_id().as_expr().eq(ParentOuterRef::id().as_lateral_outer_expr()))
        .limit(1);

    let lat = outer.left_join_lateral(inner);
    let sql = lat.__sql_for_test().unwrap();

    assert!(sql.contains("LEFT JOIN LATERAL ("), "left join lateral: {}", sql);
    assert!(sql.contains("TRUE AS __djogi_lateral_present"), "sentinel projection: {}", sql);
}

#[test]
fn test_count_lateral_shape() {
    let outer = Parent::objects();
    let inner = Child::objects().limit(3);

    let lat = outer.join_lateral(inner);
    let sql = lat.__count_sql_for_test().unwrap();

    assert!(sql.starts_with("SELECT COUNT(*) FROM (SELECT "), "count wraps everything: {}", sql);
    assert!(sql.ends_with(")"), "count closes parens: {}", sql);
    // Inner limit must be preserved inside the parens
    assert!(sql.contains("LIMIT $1"), "inner limit preserved: {}", sql);
}
