// Phase 8.5 — #171: unsupported array element type must be rejected.
//
// `Vec<u32>` is not in the `IntoArrayFilterValue` sealed set.  The
// model macro accepts the field declaration (it maps to
// `FieldSqlType::Custom("...")` since u32 has no DjogiSqlType impl),
// but calling array operators on it must fail because `u32` does not
// implement `IntoArrayFilterValue`.
use djogi::prelude::*;

#[model(table = "t")]
#[derive(Debug, Clone)]
pub struct T {
    pub unsupported: Vec<u32>,
}

fn _trigger_error() {
    let vals: Vec<u32> = vec![1, 2];
    let _ = T::objects()
        .filter(|f| f.unsupported().explicit_pg_predicate().contains(&vals));
}

fn main() {}
