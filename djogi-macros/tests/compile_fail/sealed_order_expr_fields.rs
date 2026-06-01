// The `OrderExpr::Column` variant is sealed against downstream fabrication.
//
// `OrderExpr` is a `#[non_exhaustive]` enum (promoted from struct in T3).
// The `Column` variant carries the `#[non_exhaustive]` attribute, so attempting
// to construct it with a struct expression from outside the crate fails with
// E0639 ("cannot create non-exhaustive variant using struct expression").
//
// This is the actual seal: `#[non_exhaustive]` on the variant, not `pub(crate)`
// on the fields. The variant fields themselves are public so the emitter can
// read them, but external construction is blocked by the non-exhaustive gate.
//
// This test pins the seal: attempting to name the variant in downstream
// construction must fail to compile with E0639, preventing callers from
// injecting SQL-injection payloads directly into the column emitter path.
use djogi::prelude::*;
use djogi::query::OrderExpr;

#[model(table = "posts_order_seal_test")]
#[derive(Debug, Clone)]
pub struct Post {
    pub title: String,
}

fn main() {
    // This must not compile — `OrderExpr::Column` is `#[non_exhaustive]`,
    // so constructing it with a struct expression from outside the crate
    // fails with E0639 ("cannot create non-exhaustive variant using struct
    // expression"). The variant fields themselves are public, but the
    // non-exhaustive gate blocks external struct-literal construction.
    let _ = OrderExpr::Column {
        column: "title) OR 1=1 --",
        direction: djogi::query::Direction::Asc,
        nulls: djogi::query::NullsOrder::Default,
    };
}
