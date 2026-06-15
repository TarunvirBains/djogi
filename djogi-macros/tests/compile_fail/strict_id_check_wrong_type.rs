// djogi#189 — `#[field(strict_id_check)]` on a
// non-applicable column type is rejected at parse time.
//
// The structural CHECK only applies to HeerId / RanjId family scalars
// and to relation fields (FK / O2O). A bare `String` field carries no
// HeeRanjID bit-layout invariant; the projection would silently drop
// the CHECK, which is a poor UX for an explicit opt-in. The macro
// surfaces the type mismatch with a span-precise diagnostic pointing
// at the offending attribute.

use djogi::prelude::*;

#[model(table = "wrong_strict_id_189", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct WrongStrictId189 {
 #[field(strict_id_check)]
 pub label: String,
}

fn main() {}
