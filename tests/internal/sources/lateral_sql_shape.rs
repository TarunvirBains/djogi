use djogi::prelude::*;

#[model(table = "lateral_parents", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Parent {
    pub name: String,
    pub active: bool,
}

#[model(table = "lateral_children", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Child {
    pub parent_id: HeerId,
    pub score: i64,
}

#[model(table = "lateral_other_parents", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct OtherParent {
    pub name: String,
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
    assert!(
        sql.contains("FROM (SELECT "),
        "outer must be a derived source select: {}",
        sql
    );
    assert!(
        sql.contains("FROM lateral_parents WHERE active = $1"),
        "outer filter must stay inside derived source: {}",
        sql
    );
    assert!(
        sql.contains(") AS l JOIN LATERAL ("),
        "derived outer alias must feed lateral join: {}",
        sql
    );
    assert!(sql.contains("JOIN LATERAL ("), "inner join lateral: {}", sql);
    assert!(sql.contains("FROM lateral_children"), "inner from: {}", sql);
    assert!(sql.contains("WHERE parent_id = l.id"), "outer ref correlation: {}", sql);
    assert!(
        sql.contains("ORDER BY score DESC LIMIT $2"),
        "inner limit must follow outer bind(s): {}",
        sql
    );
    assert!(sql.contains(") AS r ON TRUE"), "on true clause: {}", sql);
    assert!(
        !sql.contains("ON TRUE WHERE l.active"),
        "outer predicates should not be appended after lateral fan-out: {}",
        sql
    );
}

#[test]
fn test_left_lateral_shape() {
    let outer = Parent::objects();
    let inner = Child::objects()
        .filter_expr(|f| f.parent_id().as_expr().eq(ParentOuterRef::id().as_lateral_outer_expr()))
        .limit(1);

    let lat = outer.left_join_lateral(inner);
    let sql = lat.__sql_for_test().unwrap();

    assert!(
        sql.contains("FROM (SELECT "),
        "left lateral outer must also be a derived source: {}",
        sql
    );
    assert!(sql.contains("LEFT JOIN LATERAL ("), "left join lateral: {}", sql);
    assert!(sql.contains("TRUE AS __djogi_lateral_present"), "sentinel projection: {}", sql);
}

#[test]
fn test_left_lateral_none_inner_forces_empty_subquery() {
    let outer = Parent::objects();
    let inner = Child::objects().none();

    let lat = outer.left_join_lateral(inner);
    let sql = lat.__sql_for_test().unwrap();

    assert!(
        sql.contains("LEFT JOIN LATERAL (SELECT "),
        "left join lateral shape: {}",
        sql
    );
    assert!(
        sql.contains("FROM lateral_children WHERE FALSE"),
        "none() inner must force an empty subquery: {}",
        sql
    );
    assert!(sql.contains(") AS r ON TRUE"), "join alias preserved: {}", sql);
}

#[test]
fn test_lateral_outer_ref_rejected_outside_lateral_scope() {
    let err = Child::objects()
        .filter_expr(|f| {
            f.parent_id()
                .as_expr()
                .eq(ParentOuterRef::id().as_lateral_outer_expr())
        })
        .__sql_for_test()
        .expect_err("non-lateral query should reject lateral outer refs");

    assert!(
        matches!(err, djogi::DjogiError::Predicate(_)),
        "expected predicate error, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("lateral") && msg.contains("out of scope"),
        "expected lateral out-of-scope diagnostic, got: {msg}"
    );
}

#[test]
fn test_lateral_outer_ref_model_mismatch_is_rejected() {
    let inner = Child::objects().filter_expr(|f| {
        f.parent_id()
            .as_expr()
            .eq(OtherParentOuterRef::id().as_lateral_outer_expr())
    });

    let err = Parent::objects()
        .join_lateral(inner)
        .__sql_for_test()
        .expect_err("wrong-model outer ref should fail SQL build");

    assert!(
        matches!(err, djogi::DjogiError::Predicate(_)),
        "expected predicate error, got {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("model mismatch") && msg.contains("OtherParent"),
        "expected model mismatch diagnostic, got: {msg}"
    );
}

#[test]
fn test_nested_subquery_in_lateral_inner_keeps_outer_ref_scope() {
    let inner = Child::objects()
        .filter_expr(|_| {
            Exists::new(Child::objects().filter_expr(|f| {
                f.parent_id()
                    .as_expr()
                    .eq(ParentOuterRef::id().as_lateral_outer_expr())
            }))
            .as_expr()
        })
        .limit(1);

    let sql = Parent::objects().join_lateral(inner).__sql_for_test().unwrap();

    assert!(
        sql.contains(
            "EXISTS (SELECT 1 FROM lateral_children WHERE parent_id = l.id)"
        ),
        "nested subquery should still see lateral outer alias: {}",
        sql
    );
}

#[test]
fn test_count_lateral_shape() {
    let outer = Parent::objects().order_by(|f| f.name().asc()).limit(2).offset(1);
    let inner = Child::objects()
        .filter_expr(|f| f.parent_id().as_expr().eq(ParentOuterRef::id().as_lateral_outer_expr()))
        .limit(3);

    let lat = outer.join_lateral(inner);
    let sql = lat.__count_sql_for_test().unwrap();

    assert!(
        sql.starts_with("SELECT COUNT(*) FROM (SELECT "),
        "count wraps everything: {}",
        sql
    );
    assert!(sql.ends_with(")"), "count closes parens: {}", sql);
    assert!(
        sql.contains(
            "FROM lateral_parents ORDER BY name ASC LIMIT $1 OFFSET $2) AS l"
        ),
        "outer order/limit/offset must survive inside derived source: {}",
        sql
    );
    // Outer binds are consumed first; inner LIMIT follows them.
    assert!(
        sql.contains("LIMIT $3"),
        "inner limit bind must be renumbered after outer binds: {}",
        sql
    );
}

#[test]
fn test_outer_distinct_on_is_preserved_inside_derived_source() {
    let outer = Parent::objects()
        .distinct_on(|f| f.name())
        .order_by(|f| f.name().asc());
    let inner = Child::objects()
        .filter_expr(|f| f.parent_id().as_expr().eq(ParentOuterRef::id().as_lateral_outer_expr()))
        .limit(1);

    let sql = outer.join_lateral(inner).__sql_for_test().unwrap();

    assert!(
        sql.contains("FROM (SELECT DISTINCT ON (name) "),
        "outer DISTINCT ON must survive in source subquery: {}",
        sql
    );
    assert!(
        sql.contains("FROM lateral_parents ORDER BY name ASC) AS l"),
        "outer DISTINCT ON ordering must stay in source subquery: {}",
        sql
    );
}

#[test]
fn test_outer_binds_precede_inner_binds_after_renumbering() {
    let outer = Parent::objects().filter(|f| f.active().eq(true)).limit(2);
    let inner = Child::objects()
        .filter_expr(|f| f.parent_id().as_expr().eq(ParentOuterRef::id().as_lateral_outer_expr()))
        .filter(|f| f.score().gte(10))
        .limit(1);

    let sql = outer.join_lateral(inner).__sql_for_test().unwrap();

    assert!(
        sql.contains("FROM lateral_parents WHERE active = $1 LIMIT $2"),
        "outer binds must start at $1/$2 inside derived source: {}",
        sql
    );
    assert!(
        sql.contains("score >= $3"),
        "inner value binds must follow outer binds: {}",
        sql
    );
    assert!(
        sql.contains("LIMIT $4"),
        "inner limit bind must continue bind sequence: {}",
        sql
    );
}

#[test]
fn test_inner_distinct_is_preserved() {
    let outer = Parent::objects();
    let inner = Child::objects().distinct().limit(1);

    let lat = outer.join_lateral(inner);
    let sql = lat.__sql_for_test().unwrap();

    assert!(
        sql.contains("JOIN LATERAL (SELECT DISTINCT "),
        "inner distinct keyword preserved: {}",
        sql
    );
    assert!(
        sql.contains("FROM lateral_children"),
        "inner lateral source table present: {}",
        sql
    );
}
