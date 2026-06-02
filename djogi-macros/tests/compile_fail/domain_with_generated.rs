// djogi#216 — `#[field(domain = "...",
// generated = "...")]` is rejected.
//
// Postgres stored generated columns derive their column type from the
// generation expression. Combining with a domain type is technically
// valid SQL but out of Piece A scope — the macro does not validate
// domain-vs-expression type agreement. Adopters needing a generated
// column whose stored type is a domain should hand-write the migration
// via raw DDL until djogi#216 Piece B lands.

use djogi::prelude::*;

#[model(table = "prices_216_gen", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Price216Gen {
    #[field(domain = "x", generated = "value * 2")]
    pub price: rust_decimal::Decimal,
}

fn main() {}
