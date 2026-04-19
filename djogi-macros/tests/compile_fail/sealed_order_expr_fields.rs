// The `OrderExpr` fields are sealed against downstream fabrication.
//
// Prior to the Phase 3 de42874 follow-up, `OrderExpr.column` was `pub`,
// which let any downstream crate build an ordering expression whose
// column string carried SQL-injection payloads or malformed
// identifiers. That string flowed straight into
// `sqlx::QueryBuilder::push` inside `query::sql`'s `push_tail` and
// `build_count` emitters.
//
// This test pins the seal at the type system: downstream code must not
// be able to name the field when constructing or pattern-matching. The
// only supported path is `FieldRef::asc()` / `.desc()`, which itself
// goes through the sealed `FieldRef::new` constructor.
use djogi::prelude::*;
use djogi::query::OrderExpr;

#[model(table = "posts_order_seal_test")]
#[derive(Debug, Clone)]
pub struct Post {
    pub title: String,
}

fn main() {
    // This must not compile — `OrderExpr.column`, `.direction`, and
    // `.nulls` are `pub(crate)` in the djogi crate, so naming them in
    // downstream construction fails to resolve.
    let _ = OrderExpr {
        column: "title) OR 1=1 --",
        direction: djogi::query::Direction::Asc,
        nulls: djogi::query::NullsOrder::Default,
    };
}
