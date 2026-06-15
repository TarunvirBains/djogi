// non-exhaustive LookupOp wildcard coverage.
//
// sassi marks `LookupOp` as `#[non_exhaustive]`. The macro-emitted
// `Model::__djogi_emit_field_predicate` override expands in adopter
// crates; the `match (field.field_name(), field.op())` body MUST
// include per-field wildcard arms plus a final unknown-field arm so
// a future Sassi `LookupOp` variant does not break downstream
// compilation.
//
// The fixture's compile success proves the wildcards are present:
// if the macro had emitted an exhaustive match keyed off all known
// `LookupOp` variants without wildcards, rustc would reject the
// adopter-side impl block at fixture compile time citing the
// non-exhaustive enum. That a wide range of `PortableFieldKind`s —
// Scalar, String, Bool, OptionScalar, OptionString, OptionBool,
// Array (Vec<i32>) — coexist on one model and the single emitted
// `impl Model for Widget` block compiles is the lock.
//
// Safe array-equality kinds (`Vec<i32>` in this fixture) get portable
// equality/list arms plus a catch-all unsupported-lookup wildcard.
// Non-portable array element types are covered by compile-fail fixtures.
//
// Per the lihaaf compile-fixture contract, every lihaaf fixture has
// `fn main` so the binary still has to link.

use djogi::prelude::*;

#[model(table = "phase8eta_lookup_op_widgets")]
#[derive(Debug, Clone)]
pub struct Widget {
 // Scalar — exercises Eq/Neq/Gt/Gte/Lt/Lte/Between/In/NotIn arms.
 pub price: i64,
 // String — adds the LIKE/ILIKE pattern family.
 pub name: String,
 // Bool — equality / list arms only.
 pub active: bool,
 // OptionScalar — adds null-test arms and option-aware
 // eq/neq/in/not_in dispatch.
 pub estimated_year: Option<i32>,
 // OptionString — same surface as OptionScalar minus pattern
 // arms.
 pub maybe_label: Option<String>,
 // OptionBool — null tests + scalar list/eq arms.
 pub maybe_flag: Option<bool>,
 // Array with safe portable element equality. Emits Eq/Neq/In/NotIn
 // arms plus a catch-all unsupported-lookup wildcard.
 pub tags: Vec<i32>,
}

fn main() {}
