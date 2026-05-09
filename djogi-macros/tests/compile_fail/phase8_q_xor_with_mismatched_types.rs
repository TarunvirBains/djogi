// Phase 8γ T6.12 — `Q<A> ^ Q<B>` MUST fail.
//
// The Q-algebra is parameterised in the model type — composing
// predicates across different model types is a type-level error,
// not a runtime one. This fixture verifies the compiler rejects the
// mixed expression at the BitXor impl boundary.
//
// `Q<T>` declares `impl<T: Model> BitXor for Q<T>` with
// `type Output = Q<T>`, so `Q<A> ^ Q<B>` fails because the operator
// trait expects `Self = Q<T>` on both sides.
//
// Per `feedback_trybuild_fixtures.md`, every trybuild fixture must
// have `fn main` so the stored `.stderr` does not pick up
// `E0601 (main not found)` noise.

use djogi::prelude::*;

#[model(table = "phase8_q_xor_mismatched_a")]
#[derive(Debug, Clone)]
pub struct A {
    pub x: i64,
}

#[model(table = "phase8_q_xor_mismatched_b")]
#[derive(Debug, Clone)]
pub struct B {
    pub y: i64,
}

fn main() {
    let qa: Q<A> = Q::always_true();
    let qb: Q<B> = Q::always_true();
    let _q = qa ^ qb;
}
