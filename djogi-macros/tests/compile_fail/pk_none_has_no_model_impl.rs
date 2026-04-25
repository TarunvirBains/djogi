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

#[model(table = "custom_pk", pk = None)]
#[derive(Debug, Clone)]
pub struct Custom {
    pub custom_id: String,
    pub value: String,
}

fn _must_not_compile() {
    // `Custom::create` must NOT exist for pk=none models. If it does,
    // Model impl snuck back in — regression.
    //
    // We probe `create` specifically (not `get`) because rustc's
    // "candidate trait" diff for `get` enumerates every trait in the
    // transitive dep graph with that method (Row, SliceIndex, various
    // icu_* crates, etc.), and that list shifts whenever deps update —
    // making the stored .stderr brittle. `create` has exactly one
    // candidate (`djogi::model::Model`), producing a stable diagnostic.
    let _ = Custom::create;
}

fn main() {}
