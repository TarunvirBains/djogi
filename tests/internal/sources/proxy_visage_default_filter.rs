// Internal SQL-shape assertion for proxy-model visage default filtering.
//
// A proxy model (`#[model(proxy_for = Parent, default_filter = |f| ...)]`)
// must propagate its `default_filter` into the visage queryset that the macro
// emits for it, mirroring what `QuerySet::new()` already does on the model
// side. This pins the macro-emitted `VisageQuerySet::__new()` seeding step:
// the rendered SELECT carries the proxy's default predicate in its WHERE
// clause before any adopter `.filter(...)` is applied.
//
// Why this is an internal SQL-shape test rather than a live `#[djogi_test]`
// integration test: verifying the proxy-specific WHERE clause requires the
// visage SQL builder (`__sql_for_test`), which reaches the macro-emitted
// seeding code without a live database and asserts the WHERE clause directly —
// the same approach `visage_traversal_shape` uses for visage traversal
// lowering. `Model::descriptor()` is keyed on `type_name` (the struct ident),
// which is unique across all registered models — a proxy and its base model
// each return their own descriptor even though they share a `table_name`.

use djogi::prelude::*;

#[model(table = "pvdf_users")]
#[derive(Debug, Clone)]
pub struct PvdfUser {
    #[field(expose(public))]
    pub name: String,
    pub active: bool,
}

#[model(
    table = "pvdf_users",
    proxy_for = PvdfUser,
    default_filter = |f| f.active.eq(true),
)]
#[derive(Debug, Clone)]
pub struct PvdfActiveUser {
    #[field(expose(public))]
    pub name: String,
    pub active: bool,
}

/// The proxy visage's `__new()` seeds the proxy's `default_filter` so the
/// rendered SELECT filters on `active = TRUE`. An adopter `.filter(...)` that
/// adds only a vacuous predicate must not drop the seeded default — the proxy
/// filter is the prefix no call can remove.
#[test]
fn proxy_visage_seeds_default_filter_in_where_clause() {
    let sql =
        PvdfActiveUserPublic::filter(|_f| Q::<PvdfActiveUser>::always_true()).__sql_for_test();

    assert!(
        sql.contains("WHERE"),
        "proxy visage SELECT must carry a WHERE clause from the seeded \
         default_filter; got: {sql}"
    );
    assert!(
        sql.contains("active") && sql.contains("TRUE"),
        "proxy visage WHERE clause must contain the default_filter predicate \
         `active = TRUE`; got: {sql}"
    );
}

/// The non-vacuous adopter predicate AND-composes onto the seeded default
/// rather than replacing it — both the default (`active`) and the adopter
/// term (`name`) appear in the rendered WHERE clause.
#[test]
fn proxy_visage_default_filter_and_composes_with_adopter_filter() {
    let sql = PvdfActiveUserPublic::filter(|f| f.name().eq("alice".to_string())).__sql_for_test();

    assert!(
        sql.contains("active") && sql.contains("TRUE"),
        "seeded default_filter `active = TRUE` must survive an adopter \
         `.filter(...)`; got: {sql}"
    );
    assert!(
        sql.contains("name"),
        "adopter predicate on `name` must appear in the WHERE clause; got: {sql}"
    );
}
