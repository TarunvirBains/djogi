//! Verify that `FieldRef<T, i64>::eq_values(ValuesFieldRef<f64>)` does not
//! compile. The type parameter `V` must be the same on both sides.
use djogi::prelude::*;

#[model(table = "animals", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct Animal {
 pub name: String,
}

fn main() {
 let _weights: InlineValues<(i64, f64)> = InlineValues::new(
  vec![(1_i64, 0.5_f64)],
  "w",
  ("animal_id", "score"),
 )
.unwrap();

 // animal.id() returns FieldRef<Animal, HeerIdDesc> — but the second VALUES
 // column is f64. This must not compile.
 let _bad: ValuesOn<Animal> = {
  let animal: <Animal as Model>::Fields = Default::default();
  let values: ValuesFields<(i64, f64)> = Default::default();
  // animal.id() has type FieldRef<Animal, HeerIdDesc>, values.col1() is
  // ValuesFieldRef<f64>. These are different V — compile error.
  // For simplicity, use a manually-typed FieldRef to make the mismatch
  // explicit without relying on id()'s exact type.
  let field_ref: FieldRef<Animal, i64> = unsafe { std::mem::zeroed() };
  let wrong_col: ValuesFieldRef<f64> = values.col1();
  field_ref.eq_values(wrong_col)
 };
}
