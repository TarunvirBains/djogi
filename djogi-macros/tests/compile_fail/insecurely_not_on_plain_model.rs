// `_insecurely()` methods are emitted ONLY for models that declare
// `#[model(tenant_key = "...")]`. A plain model (no tenant_key) must NOT
// have these inherent methods — they would silently compile and let callers
// bypass RLS isolation on models that never declared a tenant constraint.
//
// This fixture is a regression guard: if `crud.rs` ever accidentally emits
// the insecurely block unconditionally, `Plain::get_insecurely` will resolve
// and the compile_fail expectation no longer holds.
use djogi::prelude::*;

#[model(table = "plain")]
#[derive(Debug, Clone)]
pub struct Plain {
 pub name: String,
}

fn _must_not_compile() {
 // `Plain::get_insecurely` must NOT exist — `_insecurely` methods are
 // only emitted for tenant-keyed models.
 let _ = Plain::get_insecurely;
}

fn main() {}
