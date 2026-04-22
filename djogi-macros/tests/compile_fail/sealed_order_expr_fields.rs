// The `OrderExpr::Column` fields are sealed against downstream fabrication.
//
// `OrderExpr` is an enum (promoted from struct in Phase 6 T3). The
// `Column` variant fields (`column`, `direction`, `nulls`) are
// `pub(crate)`, so downstream code cannot construct an `OrderExpr`
// with an arbitrary column string — the only public path is
// `FieldRef::asc()` / `.desc()`, which uses the sealed `FieldRef::new`
// constructor.
//
// This test pins the seal: attempting to name the variant fields in
// downstream construction must fail to compile, preventing callers
// from injecting SQL-injection payloads directly into the column
// emitter path (`SqlAccumulator::push_sql`).
use djogi::prelude::*;
use djogi::query::OrderExpr;

#[model(table = "posts_order_seal_test")]
#[derive(Debug, Clone)]
pub struct Post {
    pub title: String,
}

fn main() {
    // This must not compile — `OrderExpr::Column.column`,
    // `.direction`, and `.nulls` are `pub(crate)`, so naming them in
    // downstream construction fails to resolve.
    let _ = OrderExpr::Column {
        column: "title) OR 1=1 --",
        direction: djogi::query::Direction::Asc,
        nulls: djogi::query::NullsOrder::Default,
    };
}
