// Eight-term mixed AND/OR/XOR/NOT composition for `Q<T>`.
//
// Locks the specificationT6 acceptance criterion: "[Compile-pass] complex
// 8-term composition with mixed XOR / AND / OR / NOT." (originally
// titled "Trybuild compile-pass" before the lihaaf
// migration.) The fixture
// exercises every operator, the portable true/false constructors, and
// the `Q::Compound` flattening contract simultaneously, ensuring no
// inference cliff appears at multi-operand chained compositions.
//
// Pre-T6.9 the `FieldRef::eq` methods still return `Condition`, so
// this fixture builds `Q<T>` operands directly from
// `Q::always_true` / `Q::always_false` and `Q::Negated` wrappers rather
// than from field-comparison return values. Operator precedence and
// flattening behavior are independent of how the operands are
// constructed; the fixture's job is to lock the algebra's
// composability under chained mixed operators.

use djogi::prelude::*;

#[model(table = "phase8_q_algebra_eight_term_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
 pub name: String,
 pub price: i64,
 pub active: bool,
 pub sku: String,
}

fn main() {
 // Eight basic operands — mix of Q portable true/false
 // sentinels and Q::Negated wrappers. The eight-term composition
 // chains AND, OR, XOR, NOT in every position.
 let t = || Q::<Widget>::always_true();
 let f = || Q::<Widget>::always_false();
 let neg = || Q::<Widget>::Negated(Box::new(t()));

 // (a & b & c) ^ (d | e | f) | !(g & h) — exercises every
 // operator at every level. Per Rust precedence: outer `|`
 // wraps the XOR result and the negation; inner `&` chains
 // flatten through `Q::Compound{op: And}`; `!` over a non-Basic
 // operand wraps in `Q::Negated`.
 let _q: Q<Widget> = (t() & neg() & f()) ^ (neg() | t() | neg()) | !(neg() & neg());
}
