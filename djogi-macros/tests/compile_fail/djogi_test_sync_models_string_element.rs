//! `#[djogi_test(sync_models = ["Widget"])]` — string literals are not
//! type paths. This is a common copy-paste mistake from the
//! `extensions = ["postgis"]` form. The macro must reject with a
//! span-precise error pointing at the offending element.

#[djogi::djogi_test(sync_models = ["Widget"])]
async fn sync_models_elements_must_be_paths(mut _ctx: djogi::DjogiContext) {}

fn main() {}
