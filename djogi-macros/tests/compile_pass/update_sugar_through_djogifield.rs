// PR3 / Finding 6 — update sugar reachable from generated `DjogiField`s.
//
// Verifies that the post-PR3 `{Model}Fields` accessor surface exposes the
// same update-assignment sugar (`set_field`, `increment`, `decrement`) as
// the older `FieldRef` path, without callers manually unwrapping SQL handles.
use djogi::prelude::*;

#[model(table = "update_sugar_djogifield", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct UpdateSugarDjogiField {
 pub some_i32: i32,
 pub other_i32: i32,
 pub label: String,
}

fn main() {
 // (1) Numeric column updates via generated `DjogiField` accessors.
 let _ = UpdateSugarDjogiField::objects().update(|f| f.some_i32().increment(1i32));
 let _ = UpdateSugarDjogiField::objects().update(|f| f.some_i32().decrement(2i32));

 // (2) Field-vs-field copy through generated accessors. `set_field` must
 // accept `DjogiField` inputs via the sealed SQL conversion path.
 let _ = UpdateSugarDjogiField::objects().update(|f| f.some_i32().set_field(f.other_i32()));
}

