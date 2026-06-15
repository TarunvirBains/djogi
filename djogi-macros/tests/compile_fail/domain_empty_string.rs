// djogi#216 — `#[field(domain = "")]` is rejected.
//
// The domain name must satisfy the Postgres unquoted-identifier byte
// shape (non-empty, ≤63 bytes, ASCII letter or underscore first, ASCII
// alphanumerics or underscores after). An empty literal fails the
// non-empty rule; the macro emits the same single-message diagnostic
// the byte-shape validator carries — pointing at the offending literal
// so the underline isolates the offender.

use djogi::prelude::*;

#[model(table = "orders_216_empty", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Order216Empty {
 #[field(domain = "")]
 pub amount: rust_decimal::Decimal,
}

fn main() {}
