// Phase 8γ T6.12 — `Q<A> & Q<B>` MUST fail (analogous to the XOR
// fixture). Locks the type-parameterization guarantee on `BitAnd`
// for `Q<T>` — composing across model types is a type-level error.
//
// Per `feedback_trybuild_fixtures.md`, every trybuild fixture must
// have `fn main` so the stored `.stderr` does not pick up
// `E0601 (main not found)`.

use djogi::prelude::*;

#[model(table = "phase8_q_and_mismatched_a")]
#[derive(Debug, Clone)]
pub struct A {
    pub x: i64,
}

#[model(table = "phase8_q_and_mismatched_b")]
#[derive(Debug, Clone)]
pub struct B {
    pub y: i64,
}

fn main() {
    let qa: Q<A> = Q::Basic(BasicPredicate::True);
    let qb: Q<B> = Q::Basic(BasicPredicate::True);
    let _q = qa & qb;
}
