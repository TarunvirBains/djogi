// Phase 8γ T6.11 — Operator precedence compile-pass for `Q<T>`.
//
// Locks Rust operator precedence at the type level: `&` > `^` > `|`
// (AND binds tighter than XOR binds tighter than OR). The expression
// `Q::from(p) ^ Q::from(q) | Q::Negated(...)` MUST parse as
// `(Q::from(p) ^ Q::from(q)) | Q::Negated(...)`.
//
// This fixture verifies the parse compiles. The runtime shape lock
// lives alongside in `query::q::tests::q_operator_precedence_*`. v3
// §T6 acceptance criterion bullet 6 + Codex review focus bullet 3.
//
// Uses `Q::Negated(Box::new(Q::always_true()))` as the non-portable
// operand so the precedence test exercises the
// mixed-operand path through `Q::Compound` / `Q::Xor`. Pre-T6.9
// the `FieldRef::eq` etc. methods still return `Condition`, so the
// fixture can't use field-method return values directly as `Q<T>`;
// the operator-precedence guarantee is independent of which
// constructors produce the operands.
//
// Per `feedback_trybuild_fixtures.md`, every trybuild fixture must
// have `fn main` so the stored `.stderr` (when compile-fail) does
// not pick up `E0601 (main not found)`. compile-pass fixtures need
// `fn main()` for the same reason — the binary still has to link.

use djogi::prelude::*;

#[model(table = "phase8_q_algebra_xor_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
    pub name: String,
    pub price: i64,
}

fn main() {
    let true_basic = || Q::<Widget>::always_true();
    let false_basic = || Q::<Widget>::always_false();
    let neg = || Q::<Widget>::Negated(Box::new(true_basic()));

    // (Basic ^ Negated) | Negated — XOR binds tighter than OR.
    // Outer must be Q::Compound{op: Or}; inner left half must be
    // Q::Xor(_, _). The runtime test
    // `q_operator_precedence_xor_binds_tighter_than_or` in
    // `query::q::tests` mirrors this and pattern-matches the
    // resulting tree shape; this fixture pins the parse only.
    let _q1: Q<Widget> = true_basic() ^ neg() | neg();

    // (Basic & Negated) ^ Negated — AND binds tighter than XOR.
    let _q2: Q<Widget> = true_basic() & neg() ^ neg();

    // (Basic & Negated) | Negated ^ Basic — full chain. Parses as
    // ((Basic & Negated) | (Negated ^ Basic)) per Rust precedence
    // (& > ^ > |). Locks the three-way bracketing.
    let _q3: Q<Widget> = true_basic() & neg() | neg() ^ false_basic();
}
