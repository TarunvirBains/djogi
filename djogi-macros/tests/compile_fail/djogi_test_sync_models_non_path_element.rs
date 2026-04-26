//! `#[djogi_test(sync_models = [42])]` — every array element must be
//! a bare type path. A non-path element should fail with a span
//! pointing at the offending token.

#[djogi::djogi_test(sync_models = [42])]
async fn sync_models_elements_must_be_paths(mut _ctx: djogi::DjogiContext) {}

fn main() {}
