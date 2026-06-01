// #101 — typed set operations: SQL shape pins.
//
// `QuerySet::union` / `.union_all(...)` / `.intersect(...)` / `.except(...)`
// build a `SetOpQuerySet<T>` whose terminal calls emit
// `(<LEFT>) <OP> (<RIGHT>) [ORDER BY ...] [LIMIT ...] [OFFSET ...]`. This
// fixture pins:
//
// 1. Each operator emits the correct Postgres keyword.
// 2. Both arms are always parenthesised so per-arm `ORDER BY` / `LIMIT`
//    are legal Postgres.
// 3. Per-arm filters bind through positional `$N`, and right-arm binds
//    are renumbered to continue the left arm's bind sequence via
//    `SqlAccumulator::extend_with`.
// 4. Outer `ORDER BY` / `LIMIT` / `OFFSET` apply to the combined result
//    (after the trailing closing paren of the right arm).
// 5. `count()` wraps the set op in `SELECT COUNT(*) FROM (...) AS sub`
//    and strips the outer ORDER BY / LIMIT / OFFSET.
// 6. Nested set-ops (`a.union(b).intersect(c)`) recurse with proper
//    parenthesisation.
// 7. Arms marked `.none()` short-circuit to `SELECT ... WHERE FALSE`
//    inside their parens so the set-op algebra remains correct.
// 8. Validation rejects every shape `validate_arm` checks for —
//    `select_for_update` locks, `.prefetch(...)` paths,
//    `.select_related(...)` paths, and `.cache(...)` bindings — each
//    with a typed `DjogiError::SetOpArmInvalid` before any SQL is
//    issued.
//
// All asserts run against the SQL builder via `__sql_for_test` and the
// crate-internal `build_set_op_count` helper. No live database is
// required.

use djogi::prelude::*;

#[model(table = "phase8_5_c4b_set_op_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
    pub name: String,
    pub score: i64,
    pub active: bool,
}

// FK target — `Book` carries `ForeignKey<Author>` so the SQL-shape
// fixture can exercise `.prefetch(...)` / `.select_related(...)` arm
// validation through the real macro-emitted `BookRelated::author()`
// relation path. The `Author` model itself plays no role in the
// validation tests beyond being the FK target — its rows are never
// created or fetched in this DB-free fixture.
#[model(table = "phase8_5_c4b_set_op_authors", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct Author {
    pub name: String,
}

// Carries a non-null FK to `Author`. `no_default` because
// `ForeignKey<T>` does not implement `Default`. The fixture never
// constructs a `Book` value; only `Book::objects()` for the queryset
// chain is exercised.
#[model(table = "phase8_5_c4b_set_op_books", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Book {
    pub title: String,
    pub author_id: ForeignKey<Author>,
}

// Spatial fixture for the outer-ORDER-BY rejection tests. The model
// is only constructed when `feature = "spatial"` is on so the rest of
// the suite remains feature-agnostic.
#[cfg(feature = "spatial")]
#[allow(dead_code)] // the fixture never constructs `Place` values.
#[model(table = "phase8_5_c4b_set_op_places", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Place {
    pub name: String,
    pub location: djogi::GeoPoint,
}

fn select_sql(sop: &SetOpQuerySet<Widget>) -> String {
    sop.__sql_for_test()
        .expect("set-op SQL builder must succeed for these shapes")
}

#[test]
fn set_op_union_emits_parenthesised_arms_and_keyword() {
    let left = Widget::objects().filter(|f| f.active().eq(true));
    let right = Widget::objects().filter(|f| f.score().gte(10i64));
    let sql = select_sql(&left.union(right));
    // Both arms parenthesised; UNION keyword between them.
    assert!(sql.starts_with("("), "left arm must be parenthesised: {sql}");
    assert!(sql.ends_with(")"), "right arm must close with `)`: {sql}");
    assert!(sql.contains(") UNION ("), "UNION must separate the arms: {sql}");
    // Left arm bind $1; right arm bind $2 after extend_with renumbering.
    assert!(sql.contains("active = $1"), "left bind $1: {sql}");
    assert!(sql.contains("score >= $2"), "right bind $2: {sql}");
}

#[test]
fn set_op_union_all_uses_union_all_keyword() {
    let left = Widget::objects().filter(|f| f.active().eq(true));
    let right = Widget::objects().filter(|f| f.active().eq(false));
    let sql = select_sql(&left.union_all(right));
    assert!(
        sql.contains(") UNION ALL ("),
        "UNION ALL keyword between arms: {sql}"
    );
    // Plain `UNION` must NOT appear as a separator — guards against
    // accidentally regressing to dedup semantics.
    let parts: Vec<&str> = sql.split(") UNION ALL (").collect();
    assert_eq!(
        parts.len(),
        2,
        "exactly one UNION ALL separator: {sql}"
    );
}

#[test]
fn set_op_intersect_uses_intersect_keyword() {
    let left = Widget::objects().filter(|f| f.score().gte(10i64));
    let right = Widget::objects().filter(|f| f.score().lte(100i64));
    let sql = select_sql(&left.intersect(right));
    assert!(
        sql.contains(") INTERSECT ("),
        "INTERSECT keyword between arms: {sql}"
    );
}

#[test]
fn set_op_except_uses_except_keyword() {
    let left = Widget::objects().filter(|f| f.score().gte(10i64));
    let right = Widget::objects().filter(|f| f.score().gte(100i64));
    let sql = select_sql(&left.except(right));
    assert!(
        sql.contains(") EXCEPT ("),
        "EXCEPT keyword between arms: {sql}"
    );
}

#[test]
fn set_op_outer_order_by_emitted_after_right_arm_close_paren() {
    let left = Widget::objects().filter(|f| f.active().eq(true));
    let right = Widget::objects().filter(|f| f.active().eq(false));
    let sop = left.union(right).order_by(|f| f.score().desc());
    let sql = select_sql(&sop);
    // Outer ORDER BY must appear AFTER the right-arm's `)` close,
    // never inside an arm.
    let close_then_order = sql.find(") ORDER BY score DESC");
    assert!(
        close_then_order.is_some(),
        "outer ORDER BY must follow the right arm's close paren: {sql}"
    );
}

#[test]
fn set_op_outer_limit_and_offset_bind_after_arm_binds() {
    let left = Widget::objects().filter(|f| f.active().eq(true));
    let right = Widget::objects().filter(|f| f.score().gte(10i64));
    let sop = left.union(right).limit(20).offset(5);
    let sql = select_sql(&sop);
    // Two arm binds ($1, $2) before the LIMIT/OFFSET binds.
    assert!(sql.contains("active = $1"), "{sql}");
    assert!(sql.contains("score >= $2"), "{sql}");
    // LIMIT and OFFSET land on $3 and $4 after the right-arm close.
    assert!(
        sql.contains(") LIMIT $3 OFFSET $4"),
        "outer LIMIT/OFFSET must follow the right-arm close paren with correct ordinals: {sql}"
    );
}

#[test]
fn set_op_per_arm_order_by_and_limit_emitted_inside_arm_parens() {
    let left = Widget::objects()
        .filter(|f| f.active().eq(true))
        .order_by(|f| f.score().desc())
        .limit(10);
    let right = Widget::objects()
        .filter(|f| f.active().eq(false))
        .order_by(|f| f.score().asc())
        .limit(5);
    let sql = select_sql(&left.union(right));
    // Per-arm ORDER BY / LIMIT must appear INSIDE the parens of their
    // arm. Postgres requires this — naked `SELECT ... ORDER BY ... UNION
    // SELECT ...` is illegal because the ORDER BY would bind to the
    // outer set op. Parenthesised arms make the per-arm syntax legal.
    //
    // Bind ordering across both arms (renumbered by extend_with):
    //   left  arm: $1 (filter active=true)  $2 (left LIMIT 10)
    //   right arm: $3 (filter active=false) $4 (right LIMIT 5)
    assert!(
        sql.contains("WHERE active = $1 ORDER BY score DESC LIMIT $2"),
        "left-arm WHERE + ORDER BY + LIMIT must emit inside left arm: {sql}"
    );
    assert!(
        sql.contains("WHERE active = $3 ORDER BY score ASC LIMIT $4"),
        "right-arm WHERE + ORDER BY + LIMIT must emit inside right arm: {sql}"
    );
    // Confirm both ORDER BY clauses live INSIDE the parens of their
    // arm by checking the close-paren follows the LIMIT $N token.
    assert!(
        sql.contains("LIMIT $2)") && sql.contains("LIMIT $4)"),
        "each arm's LIMIT must close before the arm's ): {sql}"
    );
}

#[test]
fn set_op_none_arm_short_circuits_to_where_false() {
    let left = Widget::objects().filter(|f| f.active().eq(true));
    let right = Widget::objects().none();
    let sql = select_sql(&left.union(right));
    // Left arm carries its real WHERE; right arm short-circuits to
    // `WHERE FALSE` inside its parens (no condition tree from `.none()`).
    assert!(sql.contains("active = $1"), "left arm preserved: {sql}");
    assert!(
        sql.contains("WHERE FALSE"),
        "none() right arm must short-circuit to WHERE FALSE: {sql}"
    );
    // The right-arm SELECT explicitly names the table; the empty
    // queryset must not collapse to bare parens.
    assert!(
        sql.contains("FROM phase8_5_c4b_set_op_widgets WHERE FALSE"),
        "WHERE FALSE arm must still name the table: {sql}"
    );
}

#[test]
fn set_op_count_wraps_in_subquery_and_strips_outer_modifiers() {
    let left = Widget::objects().filter(|f| f.active().eq(true));
    let right = Widget::objects().filter(|f| f.score().gte(10i64));
    let sop = left
        .union(right)
        .order_by(|f| f.score().desc())
        .limit(20)
        .offset(5);
    let (sql, bind_count) = sop
        .__count_sql_for_test()
        .expect("count SQL builder must succeed");
    // Wrapped in `SELECT COUNT(*) FROM (...) AS sub`.
    assert!(
        sql.starts_with("SELECT COUNT(*) FROM ("),
        "count must wrap in SELECT COUNT(*) FROM (...): {sql}"
    );
    assert!(sql.ends_with(") AS sub"), "count must close with `) AS sub`: {sql}");
    // The set-op body remains inside, but the OUTER ORDER BY / LIMIT /
    // OFFSET are stripped (they don't affect cardinality).
    assert!(sql.contains(") UNION ("), "set-op body preserved: {sql}");
    assert!(
        !sql.contains("ORDER BY"),
        "outer ORDER BY must be stripped from count: {sql}"
    );
    assert!(
        !sql.contains(" LIMIT "),
        "outer LIMIT must be stripped from count: {sql}"
    );
    assert!(
        !sql.contains(" OFFSET "),
        "outer OFFSET must be stripped from count: {sql}"
    );
    // Arm binds still present.
    assert_eq!(
        bind_count, 2,
        "count keeps only arm binds, not the stripped LIMIT/OFFSET binds"
    );
}

#[test]
fn set_op_nested_chain_renders_parenthesised_recursively() {
    let a = Widget::objects().filter(|f| f.score().eq(1i64));
    let b = Widget::objects().filter(|f| f.score().eq(2i64));
    let c = Widget::objects().filter(|f| f.score().eq(3i64));
    // a.union(b).intersect(c) — left-associative djogi composition,
    // produces `((a UNION b)) INTERSECT (c)` once each arm is wrapped.
    let sop = a.union(b).intersect(c);
    let sql = select_sql(&sop);
    // The outer INTERSECT separator is present.
    assert!(
        sql.contains(") INTERSECT ("),
        "outer INTERSECT separator: {sql}"
    );
    // The inner UNION appears nested — its outer parens come from
    // the nested-arm wrap, and the inner-arm parens come from the
    // inner UNION's own emission.
    assert!(
        sql.contains("(("),
        "nested set-op must wrap inner arm in extra parens: {sql}"
    );
    assert!(
        sql.contains(") UNION ("),
        "nested UNION operator preserved: {sql}"
    );
    // Bind renumbering across three arms: $1 (left of inner), $2
    // (right of inner), $3 (intersect right).
    assert!(sql.contains("score = $1"), "{sql}");
    assert!(sql.contains("score = $2"), "{sql}");
    assert!(sql.contains("score = $3"), "{sql}");
}

#[test]
fn set_op_arm_with_lock_rejected_at_build_time() {
    // `select_for_update` raises the `LockMode::ForUpdate` flag on the
    // arm queryset; `validate_arm` rejects every non-`None` lock mode.
    // `__sql_for_test` runs the same emitter path the live terminals
    // use, so the rejection surfaces here without a live DB.
    let left = Widget::objects().filter(|f| f.active().eq(true));
    let right_locked = Widget::objects()
        .filter(|f| f.active().eq(false))
        .select_for_update();
    let err = left
        .union(right_locked)
        .__sql_for_test()
        .expect_err("locked arm must be rejected at SQL build");
    let msg = format!("{err}");
    assert!(
        msg.contains("set-op arm `right`"),
        "error must identify the offending arm: {msg}"
    );
    assert!(
        msg.contains("FOR UPDATE"),
        "error must explain why the arm is incompatible: {msg}"
    );
    // Classification check — `SetOpArmInvalid` is a programming-error
    // class; pin the variant shape so a future refactor that changes
    // the rejection type would break this test loudly.
    assert!(
        matches!(err, DjogiError::SetOpArmInvalid { side, .. } if side == "right"),
        "lock-on-right-arm must surface as SetOpArmInvalid {{ side: \"right\", .. }}: {err:?}"
    );
}

#[test]
fn set_op_arm_with_select_related_rejected_at_build_time() {
    // `select_related(...)` registers a join path on the arm. Set-op
    // arms must project the canonical column list so the operator
    // sees a consistent row shape across both arms; an arm carrying
    // a join would silently change projection width.
    //
    // The `Book` model's macro-emitted `BookRelated::author()` path
    // produces the real `RelationPath<Book, Author>` shape, so this
    // exercise the validator with a production-shaped queryset (not
    // a hand-crafted poke at `select_related_paths`).
    let left = Book::objects().filter(|f| f.title().eq("a".to_string()));
    let right_with_join = Book::objects()
        .filter(|f| f.title().eq("b".to_string()))
        .select_related(BookRelated::author());
    let err = left
        .union(right_with_join)
        .__sql_for_test()
        .expect_err("select_related arm must be rejected at SQL build");
    let msg = format!("{err}");
    assert!(
        msg.contains("set-op arm `right`"),
        "error must identify the offending arm: {msg}"
    );
    assert!(
        msg.contains("select_related"),
        "error must explain why the arm is incompatible: {msg}"
    );
    assert!(
        matches!(err, DjogiError::SetOpArmInvalid { side, .. } if side == "right"),
        "select_related-on-right must surface as SetOpArmInvalid {{ side: \"right\", .. }}: {err:?}"
    );
}

#[test]
fn set_op_arm_with_prefetch_rejected_at_build_time() {
    // `prefetch(...)` registers a follow-up loader path on the arm.
    // Set-op arms cannot fan out into per-row child fetches: the
    // result of the set-op is the merged row set, and follow-up
    // prefetches would either need to run twice (once per arm) or
    // require a re-aggregated result that the set-op surface does not
    // promise. `validate_arm` rejects up front.
    //
    // Same macro-emitted `BookRelated::author()` path as the
    // `select_related` test — registered via `.prefetch(...)` instead.
    let left = Book::objects().filter(|f| f.title().eq("a".to_string()));
    let right_with_prefetch = Book::objects()
        .filter(|f| f.title().eq("b".to_string()))
        .prefetch(BookRelated::author());
    let err = left
        .union(right_with_prefetch)
        .__sql_for_test()
        .expect_err("prefetch arm must be rejected at SQL build");
    let msg = format!("{err}");
    assert!(
        msg.contains("set-op arm `right`"),
        "error must identify the offending arm: {msg}"
    );
    assert!(
        msg.contains("prefetch"),
        "error must explain why the arm is incompatible: {msg}"
    );
    assert!(
        matches!(err, DjogiError::SetOpArmInvalid { side, .. } if side == "right"),
        "prefetch-on-right must surface as SetOpArmInvalid {{ side: \"right\", .. }}: {err:?}"
    );
}

#[test]
fn set_op_arm_with_cache_rejected_at_build_time() {
    // `cache_target` rejection — the only publicly reachable path to
    // construct a `QuerySet<T>` with `cache_target = Some(_)` is the
    // `feature = "testing"` `bind_cache_for_test` helper, which
    // mirrors the crate-private `bind_cache` used by
    // `CachedPortableQuerySet`'s terminal methods. Adopters cannot
    // hit this validation arm today (the public `.cache(&punnu)`
    // returns `CachedPortableQuerySet`, not `QuerySet`, so it has no
    // `.union(...)`), but the validator branch exists as forward
    // compatibility against a future surface that exposes a
    // QuerySet-shaped cache hook directly. This test pins the
    // diagnostic so a regression of the validator branch would fail
    // loudly.
    let punnu = djogi::cache::Punnu::<Widget>::builder().build();
    let left = Widget::objects().filter(|f| f.active().eq(true));
    let right_cached = Widget::objects()
        .filter(|f| f.active().eq(false))
        .bind_cache_for_test(punnu);
    let err = left
        .union(right_cached)
        .__sql_for_test()
        .expect_err("cache-bound arm must be rejected at SQL build");
    let msg = format!("{err}");
    assert!(
        msg.contains("set-op arm `right`"),
        "error must identify the offending arm: {msg}"
    );
    assert!(
        msg.contains("cache"),
        "error must explain why the arm is incompatible: {msg}"
    );
    assert!(
        matches!(err, DjogiError::SetOpArmInvalid { side, .. } if side == "right"),
        "cache-on-right must surface as SetOpArmInvalid {{ side: \"right\", .. }}: {err:?}"
    );
}

#[test]
fn set_op_arm_validation_identifies_left_side_when_left_is_offending() {
    // Symmetry check — `validate_arm` is invoked for both arms with
    // distinct `side` strings. A left-arm violation must report
    // `side == "left"` so error messages name the right call-site.
    let left_locked = Widget::objects()
        .filter(|f| f.active().eq(true))
        .select_for_update();
    let right = Widget::objects().filter(|f| f.active().eq(false));
    let err = left_locked
        .union(right)
        .__sql_for_test()
        .expect_err("locked left arm must be rejected at SQL build");
    assert!(
        matches!(err, DjogiError::SetOpArmInvalid { side, .. } if side == "left"),
        "lock-on-left must surface as SetOpArmInvalid {{ side: \"left\", .. }}: {err:?}"
    );
}

#[test]
fn set_op_distinct_mode_on_arm_preserved_inside_arm_parens() {
    // A left arm with `.distinct()` and a plain right arm — the SELECT
    // DISTINCT keyword lives inside the left-arm parens. UNION's own
    // implicit dedup is preserved; per-arm DISTINCT is layered on top.
    let left = Widget::objects()
        .distinct()
        .filter(|f| f.active().eq(true));
    let right = Widget::objects().filter(|f| f.active().eq(false));
    let sql = select_sql(&left.union(right));
    assert!(
        sql.starts_with("(SELECT DISTINCT"),
        "left arm DISTINCT must emit inside left parens: {sql}"
    );
    // Right arm does NOT carry DISTINCT.
    let after_union = sql.split(") UNION (").nth(1).unwrap_or("");
    assert!(
        !after_union.starts_with("SELECT DISTINCT"),
        "right arm must not pick up left's DISTINCT: {after_union}"
    );
}

#[test]
fn set_op_op_accessor_returns_chosen_kind() {
    let a = Widget::objects();
    let b = Widget::objects();
    assert_eq!(a.clone().union(b.clone()).op(), SetOpKind::Union);
    assert_eq!(a.clone().union_all(b.clone()).op(), SetOpKind::UnionAll);
    assert_eq!(a.clone().intersect(b.clone()).op(), SetOpKind::Intersect);
    assert_eq!(a.except(b).op(), SetOpKind::Except);
}

#[test]
fn set_op_render_select_sql_for_testing_round_trips() {
    // The `feature = "testing"` rendering helper produces the same
    // SQL as the hidden `__sql_for_test` hook.
    let left = Widget::objects().filter(|f| f.active().eq(true));
    let right = Widget::objects().filter(|f| f.score().gte(10i64));
    let sop = left.union(right);
    let testing_sql = sop
        .render_set_op_sql_for_testing()
        .expect("testing-feature SQL renderer must succeed");
    let hidden_sql = sop
        .__sql_for_test()
        .expect("hidden test hook must succeed");
    assert_eq!(testing_sql, hidden_sql);
}

// ── Outer-ORDER-BY rejection ─────────────────────────────────────────────
//
// Postgres set-operation outer `ORDER BY` only accepts output column
// names (or position numbers), not arbitrary expressions. Djogi rejects
// expression-form outer ordering with a typed
// `DjogiError::SetOpOuterOrderingInvalid` before any SQL is sent. The
// only producer of an expression-form `OrderExpr` today is the spatial
// `order_by_distance(...)` helper, which lowers to `ST_Distance(...)`.

/// Plain column outer `ORDER BY` is accepted — the happy path is
/// preserved when the outer ordering names a real output column.
/// This is the baseline that the spatial-rejection tests below
/// contrast against.
#[test]
fn set_op_outer_order_by_plain_column_is_accepted() {
    let left = Widget::objects().filter(|f| f.active().eq(true));
    let right = Widget::objects().filter(|f| f.active().eq(false));
    let sop = left.union(right).order_by(|f| f.score().desc());
    let sql = sop
        .__sql_for_test()
        .expect("plain-column outer ORDER BY must still build successfully");
    assert!(
        sql.contains(") ORDER BY score DESC"),
        "column-form outer ORDER BY must emit unchanged: {sql}"
    );
}

#[cfg(feature = "spatial")]
#[test]
fn set_op_outer_spatial_order_by_distance_is_rejected_for_fetch_path() {
    // The fetch-shaped SQL builder must reject an outer ORDER BY that
    // carries a spatial-distance expression. Postgres would itself
    // reject this at parse time with a low-fidelity message; Djogi
    // catches it earlier and names the offending model.
    let center = djogi::GeoPoint::new(0.0, 0.0).unwrap();
    let left = Place::objects();
    let right = Place::objects();
    let err = left
        .union(right)
        .order_by(|f| f.location().order_by_distance(center))
        .__sql_for_test()
        .expect_err(
            "spatial outer ORDER BY on a set op must be rejected before SQL is built",
        );
    let msg = format!("{err}");
    assert!(
        msg.contains("set-op outer ORDER BY"),
        "error must name the offending operation: {msg}"
    );
    assert!(
        msg.contains("ST_Distance") || msg.contains("spatial distance"),
        "error must name the kind of ordering that was rejected: {msg}"
    );
    assert!(
        matches!(
            err,
            djogi::DjogiError::SetOpOuterOrderingInvalid { table, .. }
                if table == "phase8_5_c4b_set_op_places"
        ),
        "spatial outer ORDER BY must surface as SetOpOuterOrderingInvalid: {err:?}"
    );
}

#[cfg(feature = "spatial")]
#[test]
fn set_op_outer_spatial_order_by_distance_is_rejected_for_count_path() {
    // The count emitter strips outer ORDER BY before producing SQL,
    // but `count` and `fetch_all` must reject the same set-of-set-ops
    // shapes consistently: a user whose `.fetch_all(...)` errors with
    // `SetOpOuterOrderingInvalid` must also see that error on
    // `.count(...)` of the same value, never a silent success. Pin
    // the consistency here by exercising the count emitter directly.
    let center = djogi::GeoPoint::new(0.0, 0.0).unwrap();
    let sop = Place::objects()
        .union(Place::objects())
        .order_by(|f| f.location().order_by_distance(center));
    let err = sop
        .__count_sql_for_test()
        .expect_err(
            "spatial outer ORDER BY must be rejected on the count path too",
        );
    assert!(
        matches!(err, djogi::DjogiError::SetOpOuterOrderingInvalid { .. }),
        "count emitter must surface the same SetOpOuterOrderingInvalid as fetch: {err:?}"
    );
}

#[cfg(feature = "spatial")]
#[test]
fn set_op_per_arm_spatial_order_by_distance_is_accepted() {
    // Per-arm spatial ordering is legal — the arm's ordering lives
    // INSIDE the arm parens, where Postgres allows expression ORDER
    // BY. Only the outer ordering is rejected. This pin guards
    // against an over-eager validator that rejects spatial ordering
    // anywhere on the queryset.
    let center = djogi::GeoPoint::new(0.0, 0.0).unwrap();
    let left = Place::objects().order_by(|f| f.location().order_by_distance(center));
    let right = Place::objects().order_by(|f| f.location().order_by_distance(center));
    // Build the union and confirm the SQL emits the per-arm
    // ST_Distance(...) without erroring out — both arms have spatial
    // ordering inside their own parens.
    let sql = left
        .union(right)
        .__sql_for_test()
        .expect("per-arm spatial ORDER BY must build successfully");
    assert!(
        sql.contains("ST_Distance("),
        "per-arm ST_Distance must emit inside arm parens: {sql}"
    );
    // Make sure the outer ORDER BY is empty — no trailing ORDER BY
    // after the right arm's close paren.
    assert!(
        !sql.contains(") ORDER BY ST_Distance"),
        "no outer ORDER BY when only the arms carry spatial ordering: {sql}"
    );
}
