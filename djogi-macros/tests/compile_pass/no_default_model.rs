// Proves `#[model(no_default)]` compiles for a struct with a non-Default
// user-field type (`djogi::Date`, which aliases `time::Date`). Without the
// flag, the generated `impl Default for Product` would fail with E0277
// because time::Date does not implement Default.
//
// Uses `djogi::Date` (not `::time::Date`) so this fixture doesn't need
// `time` as a direct dep — `djogi` already re-exports it via `djogi::types`.
use djogi::prelude::*;

// T2 flipped the default PK to `HeerIdRecencyBiased`;
// explicit `pk = HeerId` keeps this fixture on the ascending-HeerId path
// that its `HeerId::from_i64(0)` construction exercises.
#[model(table = "products", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Product {
 pub name: String,
 pub launch_date: Date,
}

// Default::default() must NOT be available for this model; construction is
// explicit. This confirms the macro did skip the impl. We use `Date::MIN`
// as a sentinel — any valid `Date` value would work.
fn _explicit_construction() {
 let _p = Product {
  id: HeerId::from_i64(0).unwrap(),
  created_at: DateTime::UNIX_EPOCH,
  updated_at: DateTime::UNIX_EPOCH,
  name: "thing".into(),
  launch_date: Date::MIN,
 };
}

fn main() {}
