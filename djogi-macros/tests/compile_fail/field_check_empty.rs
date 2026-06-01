// djogi#105 — `#[field(check = "")]` is rejected.
//
// The CHECK expression must be non-empty / non-whitespace-only. An empty
// literal would lower to `CHECK ()` in DDL, which is invalid Postgres SQL
// and would only surface as an obscure failure at migration apply time.
// The macro rejects the empty literal at parse time with a span-precise
// diagnostic pointing at the offending string.

use djogi::prelude::*;

#[model(table = "animals_105_empty", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Animal105Empty {
    #[field(check = "")]
    pub weight_kg: f64,
}

fn main() {}
