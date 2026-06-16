// Issue #462 — cross-model set operations: SQL shape pins (DB-free).

use djogi::prelude::*;

#[model(table = "x462_shape_logins", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Login { pub actor: String, pub recent: bool }

#[model(table = "x462_shape_edits", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Edit { pub actor: String, pub recent: bool }

#[model(table = "x462_shape_activity", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Activity { pub actor: String, pub recent: bool }

#[model(table = "x462_shape_authors", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Author { pub name: String }

#[model(table = "x462_shape_books", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Book { pub title: String, pub author_id: ForeignKey<Author> }

// Visage-arm models for cross-schema test (Option F)
#[model(table = "x462_shape_msg_events", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct MsgEvent {
    #[field(expose(public))] pub actor: String,
    #[field(expose(public))] pub occurred: i32,
    pub body: String, // NOT exposed
}

#[model(table = "x462_shape_rxn_events", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct RxnEvent {
    #[field(expose(public))] pub actor: String,
    #[field(expose(public))] pub occurred: i32,
    pub emoji: String, // NOT exposed
}

// ── Operator keyword tests ──

#[test]
fn cross_union_emits_keyword_parenthesised_arms_and_both_tables() {
    let left = Login::objects().filter(|f| f.recent().eq(true));
    let right = Edit::objects().filter(|f| f.recent().eq(true));
    let sql = djogi::query::union_as::<Activity, _, _>(left, right)
        .render_cross_set_op_sql_for_testing().unwrap();

    assert!(sql.contains("UNION"), "{sql}");
    assert!(sql.contains("x462_shape_logins"), "left arm table: {sql}");
    assert!(sql.contains("x462_shape_edits"), "right arm table: {sql}");
    assert!(sql.contains("$1") && sql.contains("$2"), "renumbered binds: {sql}");
    assert!(sql.starts_with('('), "{sql}");
    assert!(sql.contains(") UNION ("), "{sql}");
}

#[test]
fn cross_union_all_uses_union_all_keyword() {
    let sql = djogi::query::union_all_as::<Activity, _, _>(Login::objects(), Edit::objects())
        .render_cross_set_op_sql_for_testing().unwrap();
    assert!(sql.contains(") UNION ALL ("), "{sql}");
}

#[test]
fn cross_intersect_uses_intersect_keyword() {
    let sql = djogi::query::intersect_as::<Activity, _, _>(Login::objects(), Edit::objects())
        .render_cross_set_op_sql_for_testing().unwrap();
    assert!(sql.contains(") INTERSECT ("), "{sql}");
}

#[test]
fn cross_except_uses_except_keyword() {
    let sql = djogi::query::except_as::<Activity, _, _>(Login::objects(), Edit::objects())
        .render_cross_set_op_sql_for_testing().unwrap();
    assert!(sql.contains(") EXCEPT ("), "{sql}");
}

// ── Outer modifiers ──

#[test]
fn cross_outer_order_by_and_limit_offset_apply_to_combined_result() {
    let sql = djogi::query::union_as::<Activity, _, _>(Login::objects(), Edit::objects())
        .order_by("actor", OuterOrder::Asc)
        .limit(3).offset(1)
        .render_cross_set_op_sql_for_testing().unwrap();
    let order_idx = sql.find("ORDER BY").expect("order by present");
    let last_union = sql.rfind("UNION").expect("union present");
    assert!(order_idx > last_union, "outer ORDER BY after operator: {sql}");
    assert!(sql.contains("ORDER BY actor ASC"), "{sql}");
    assert!(sql.contains("LIMIT"), "{sql}");
    assert!(sql.contains("OFFSET"), "{sql}");
}

#[test]
fn cross_count_wraps_in_subquery_and_strips_outer_modifiers() {
    let (sql, _binds) = djogi::query::union_as::<Activity, _, _>(Login::objects(), Edit::objects())
        .order_by("actor", OuterOrder::Asc).limit(2).offset(1)
        .__count_sql_for_test().unwrap();
    assert!(sql.starts_with("SELECT COUNT(*) FROM ("), "{sql}");
    assert!(sql.trim_end().ends_with(") AS sub"), "{sql}");
    assert!(!sql.contains("LIMIT"), "count strips outer LIMIT: {sql}");
    assert!(!sql.contains("OFFSET"), "count strips outer OFFSET: {sql}");
    assert!(!sql.contains("ORDER BY"), "count strips outer ORDER BY: {sql}");
}

// ── Arm rejection tests ──

#[test]
fn cross_arm_with_lock_rejected_at_build_time() {
    let plain = Login::objects().filter(|f| f.recent().eq(true));
    let locked = Edit::objects().filter(|f| f.recent().eq(true)).select_for_update();
    let err = djogi::query::union_as::<Activity, _, _>(plain, locked)
        .render_cross_set_op_sql_for_testing().unwrap_err();
    assert!(matches!(err, DjogiError::SetOpArmInvalid { side, .. } if side == "right"),
        "lock on right arm: {err:?}");
}

#[test]
fn cross_arm_validation_identifies_left_side() {
    let locked = Login::objects().select_for_update();
    let plain = Edit::objects();
    let err = djogi::query::union_as::<Activity, _, _>(locked, plain)
        .render_cross_set_op_sql_for_testing().unwrap_err();
    assert!(matches!(err, DjogiError::SetOpArmInvalid { side, .. } if side == "left"),
        "lock on left arm: {err:?}");
}

#[test]
fn cross_arm_with_select_related_rejected_at_build_time() {
    let with_join = Book::objects().select_related(BookRelated::author());
    let plain = Login::objects();
    let err = djogi::query::union_as::<Activity, _, _>(with_join, plain)
        .render_cross_set_op_sql_for_testing().unwrap_err();
    assert!(matches!(err, DjogiError::SetOpArmInvalid { side, reason, .. }
            if side == "left" && reason.contains("select_related")),
        "select_related arm rejected: {err:?}");
}

#[test]
fn cross_outer_order_by_non_identifier_rejected() {
    let err = djogi::query::union_as::<Activity, _, _>(Login::objects(), Edit::objects())
        .order_by("actor); DROP TABLE x", OuterOrder::Asc)
        .render_cross_set_op_sql_for_testing().unwrap_err();
    assert!(matches!(err, DjogiError::SetOpOuterOrderingInvalid { .. }),
        "non-identifier rejected: {err:?}");
}

#[test]
fn cross_none_arm_short_circuits_to_where_false() {
    let left = Login::objects().filter(|f| f.recent().eq(true));
    let right = Edit::objects().none();
    let sql = djogi::query::union_as::<Activity, _, _>(left, right)
        .render_cross_set_op_sql_for_testing().unwrap();
    assert!(sql.contains("recent = $1"), "left arm condition: {sql}");
    assert!(sql.contains("WHERE FALSE"), "none() → WHERE FALSE: {sql}");
    assert!(sql.contains("FROM x462_shape_edits WHERE FALSE"),
        "WHERE FALSE names table: {sql}");
}

#[test]
fn cross_arm_with_prefetch_rejected_at_build_time() {
    let with_prefetch = Book::objects().prefetch(BookRelated::author());
    let plain = Login::objects();
    let err = djogi::query::union_as::<Activity, _, _>(with_prefetch, plain)
        .render_cross_set_op_sql_for_testing().unwrap_err();
    assert!(matches!(err, DjogiError::SetOpArmInvalid { side, reason, .. }
            if side == "left" && reason.contains("prefetch")),
        "prefetch rejected: {err:?}");
}

#[test]
fn cross_arm_with_cache_rejected_at_build_time() {
    let punnu = djogi::cache::Punnu::<Edit>::builder().build();
    let plain = Login::objects();
    let cached = Edit::objects().filter(|f| f.recent().eq(true))
        .bind_cache_for_test(punnu);
    let err = djogi::query::union_as::<Activity, _, _>(plain, cached)
        .render_cross_set_op_sql_for_testing().unwrap_err();
    assert!(matches!(err, DjogiError::SetOpArmInvalid { side, reason, .. }
            if side == "right" && reason.contains("cache")),
        "cache rejected: {err:?}");
}

// ── Option F (visage arms): cross-schema tests ──

#[test]
fn cross_union_visage_arms_emit_narrowed_projection_not_full_model() {
    let left = MsgEventPublic::filter(|f| f.occurred().gte(10i32));
    let right = RxnEventPublic::filter(|f| f.occurred().gte(10i32));
    let sql = djogi::query::union_as::<MsgEventPublic, _, _>(left, right)
        .render_cross_set_op_sql_for_testing().unwrap();

    assert!(sql.contains(") UNION ("), "{sql}");
    assert!(sql.contains("x462_shape_msg_events"), "left visage table: {sql}");
    assert!(sql.contains("x462_shape_rxn_events"), "right visage table: {sql}");
    assert!(sql.contains("actor"), "exposed actor projected: {sql}");
    assert!(sql.contains("occurred"), "exposed occurred projected: {sql}");
    assert!(!sql.contains("body"), "`body` must NOT appear: {sql}");
    assert!(!sql.contains("emoji"), "`emoji` must NOT appear: {sql}");
}

#[test]
fn cross_union_all_visage_arms_with_outer_order_compose() {
    let left = RxnEventPublic::filter(|f| f.actor().eq("ann".to_string()));
    let right = MsgEventPublic::filter(|f| f.actor().eq("ann".to_string()));
    let sql = djogi::query::union_all_as::<RxnEventPublic, _, _>(left, right)
        .order_by("occurred", OuterOrder::Desc)
        .render_cross_set_op_sql_for_testing().unwrap();
    assert!(sql.contains(") UNION ALL ("), "{sql}");
    assert!(sql.contains("$1") && sql.contains("$2"), "renumbered binds: {sql}");
    assert!(sql.contains("ORDER BY occurred DESC"), "{sql}");
}
