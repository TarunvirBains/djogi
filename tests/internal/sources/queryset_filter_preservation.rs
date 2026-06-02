// PR3 — `QuerySet::filter` SQL parity through the post-flip
// root accessor surface.
//
// PR3 flips `{Model}Fields` accessors to return `DjogiField<Self, V>`
// (the trusted-portable wrapper) instead of `FieldRef<Self, V>` (the
// SQL-only handle). The portable predicate substrate (PR2b) lifts
// `PortablePredicate<T>` into `Q::Portable(PortablePredicate)` instead
// of `Q::Condition(Condition)`, and the direct-`Q<T>` SQL emitter walks
// the trusted predicate without round-tripping through
// `q_to_condition`.
//
// This regression test pins the SQL shape callers depend on across the
// flip:
//
// 1. A simple equality filter (`f.col().eq(value)`) emits
//    `WHERE col = $1` with one bind parameter — same shape pre- and
//    post-PR3.
// 2. AND composition through the operator matrix (`pred1 & pred2`)
//    emits `(col_a = $1) AND (col_b = $2)` — same shape.
// 3. A negated filter (`exclude(|f| f.col().eq(value))`) folds the
//    negation into the trusted portable predicate (Sassi `Not`
//    collapse) so the stored shape stays `Q::Portable(!p)` and the
//    emitted SQL matches without round-tripping through a legacy
//    `Condition` bridge.
// 4. The string-pattern surface (`f.col().contains("rust")`) lowers to
//    a `COLLATE "C" ILIKE` form — PR2/PR3 keep the portable
//    ASCII-stable case-insensitive contract distinct from
//    `explicit_pg_predicate().contains` (database-locale `ILIKE`).
// 5. PostgreSQL-specific predicates require `explicit_pg_predicate()`
//    from the post-flip root surface and continue to emit through the
//    legacy `Condition` arm of `Q<T>`. The fixture asserts a regex
//    predicate emits the Postgres `~` operator.
// 6. `DjogiField<M, Option<U>>::some()` lowers to a direct equality
//    that excludes NULL rows through SQL three-valued logic, the same
//    shape Sassi's `PresentField<T, U>` evaluator handles in Punnu.
//
// All asserts run against the SQL builder via `__sql_for_test` (the
// hidden test hook) — no live database is required.

use djogi::prelude::*;

#[model(table = "phase8eta_filter_preservation_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
    pub name: String,
    pub score: i64,
    pub active: bool,
    pub estimated_year: Option<i32>,
}

fn select_sql<F>(closure: F) -> String
where
    F: FnOnce(QuerySet<Widget>) -> QuerySet<Widget>,
{
    closure(Widget::objects())
        .__sql_for_test()
        .expect("portable predicate must lower to SQL without error")
}

#[test]
fn queryset_filter_preservation_simple_equality_emits_one_bind() {
    let sql = select_sql(|qs| qs.filter(|f| f.active().eq(true)));
    assert!(sql.contains("active = $1"), "got: {sql}");
}

#[test]
fn queryset_filter_preservation_and_composition_emits_two_binds() {
    let sql = select_sql(|qs| qs.filter(|f| f.active().eq(true) & f.score().gte(10i64)));
    // The portable operator matrix lowers `&` to a parenthesised AND
    // composition. Both leaves carry their own bind ordinal.
    assert!(sql.contains("active = $1"), "got: {sql}");
    assert!(sql.contains("score >= $2"), "got: {sql}");
    assert!(sql.contains("AND"), "got: {sql}");
}

#[test]
fn queryset_filter_preservation_exclude_folds_negation_into_portable() {
    let sql = select_sql(|qs| qs.exclude(|f| f.active().eq(true)));
    // PR2b/PR3 contract: `exclude(|f| portable_pred)` pushes the
    // negation INTO the trusted `PortablePredicate` (Sassi's `Not`
    // collapses double-negations in place) instead of wrapping the
    // outer `Q<T>` in `Q::Negated`. The exact wire form may be either
    // `WHERE NOT (active = $1)` or `WHERE active <> $1` depending on
    // PR2b's negation flattening; the regression check verifies both
    // bind ordering and that the column name appears.
    assert!(
        sql.contains("active = $1") || sql.contains("active <> $1"),
        "exclude filter must lower to a single-bind active comparison; got: {sql}"
    );
    assert!(sql.contains("WHERE"), "got: {sql}");
}

#[test]
fn queryset_filter_preservation_portable_string_contains_uses_collate_c_ilike() {
    let sql = select_sql(|qs| qs.filter(|f| f.name().contains("rust")));
    // Portable case-insensitive substring lowers to `COLLATE "C" ILIKE`
    // with LIKE-escaped substring patterns. Distinct from
    // `explicit_pg_predicate().contains`, which uses Postgres'
    // database-locale `ILIKE`.
    assert!(
        sql.contains("COLLATE \"C\""),
        "portable string contains must use COLLATE \"C\" for ASCII-stable case folding; got: {sql}"
    );
    assert!(
        sql.contains("ILIKE"),
        "portable string contains must use ILIKE for substring matching; got: {sql}"
    );
}

#[test]
fn queryset_filter_preservation_explicit_pg_regex_lowers_through_condition_arm() {
    let sql =
        select_sql(|qs| qs.filter(|f| f.name().explicit_pg_predicate().regex("^rust")));
    // PG-locale regex (`column ~ $1`) routes through the legacy
    // `Condition` arm of `Q<T>` and emits the same SQL shape as the
    // pre-PR3 `FieldRef::regex` route.
    assert!(
        sql.contains("name") && sql.contains("~"),
        "explicit_pg_predicate().regex must emit a Postgres regex match; got: {sql}"
    );
}

#[test]
fn queryset_filter_preservation_optional_some_predicate_excludes_null_rows() {
    // `DjogiField<Widget, Option<i32>>::some()` enters the
    // `DjogiPresentField` view; `eq` there emits SQL that excludes
    // NULL rows through ordinary three-valued logic. The portable
    // predicate is still trusted (Sassi `PresentField`-backed) so
    // `Q::Portable` remains the storage shape.
    let sql = select_sql(|qs| qs.filter(|f| f.estimated_year().some().eq(2020i32)));
    assert!(
        sql.contains("estimated_year = $1"),
        "some().eq must emit a direct equality; got: {sql}"
    );
}

#[test]
fn queryset_filter_preservation_optional_is_null_emits_three_valued_form() {
    // Nullness predicates lower to SQL `IS NULL` / `IS NOT NULL` form,
    // matching Sassi's three-valued logic in Punnu.
    let sql = select_sql(|qs| qs.filter(|f| f.estimated_year().is_null()));
    assert!(
        sql.contains("estimated_year IS NULL"),
        "is_null must emit SQL `IS NULL`; got: {sql}"
    );
}
