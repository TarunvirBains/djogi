//! Verify that there is no `join_values` overload that accepts a raw SQL
//! string as the ON predicate. The only accepted form is the typed closure
//! that returns `ValuesOn<T>`.
use djogi::prelude::*;

#[model(table = "animals", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct Animal {
 pub name: String,
}

fn main() {
 let weights: InlineValues<(i64, f64)> = InlineValues::new(
  vec![(1_i64, 0.5_f64)],
  "w",
  ("animal_id", "score"),
 )
.unwrap();
 // Attempting to pass a raw string as ON must fail to compile.
 let _bad = Animal::objects().join_values(weights, "w.animal_id = animals.id");
}
