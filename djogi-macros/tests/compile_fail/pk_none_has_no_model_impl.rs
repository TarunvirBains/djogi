// pk="none" models intentionally get NO `impl Model` (see
// djogi-macros/src/model/crud.rs — the early return for PkStrategy::None).
// This fixture is a regression test: if a future change accidentally
// starts emitting CRUD methods for pk=none, `Custom::create` and
// `Custom::get` will resolve and the compile_fail expectation will no
// longer match.
//
// The fixture doesn't actually need a database pool; we reference
// Model-trait methods through compile-time resolution only.
use djogi::prelude::*;

#[model(table = "custom_pk", pk = "none")]
#[derive(Debug, Clone)]
pub struct Custom {
    pub custom_id: String,
    pub value: String,
}

fn _must_not_compile() {
    // `Custom::create` and `Custom::get` must NOT exist for pk=none
    // models. If they do, Model impl snuck back in — regression.
    let _ = Custom::create;
    let _ = Custom::get;
}

fn main() {}
