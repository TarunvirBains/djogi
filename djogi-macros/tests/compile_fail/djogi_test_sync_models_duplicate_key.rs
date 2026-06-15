//! `#[djogi_test(sync_models = [Widget], sync_models = [Other])]` — the same
//! `sync_models` key specified twice. The macro must reject and point the
//! caret at the *second* occurrence (per a past review span-precision
//! lock-in: unit-test text coverage existed but no lihaaf fixture
//! anchored the caret column).

#[djogi::djogi_test(sync_models = [Widget], sync_models = [Other])]
async fn duplicate_sync_models_key(mut _ctx: djogi::DjogiContext) {}

fn main() {}
