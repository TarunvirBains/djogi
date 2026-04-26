//! `#[djogi_test(sync_models = Widget)]` — the value must be an array
//! literal. A bare type path is a common authoring mistake; the macro
//! must point at the correct shape.

#[djogi::djogi_test(sync_models = Widget)]
async fn sync_models_must_be_array(mut _ctx: djogi::DjogiContext) {}

fn main() {}
