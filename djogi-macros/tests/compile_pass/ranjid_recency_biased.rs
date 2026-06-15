// `pk = RanjIdRecencyBiased` mirrors the HeerId
// variant's lowering path: the public alias collapses to the internal
// `PkStrategy::RanjIdDesc`, which injects `id: RanjIdDesc`.
use djogi::prelude::*;

#[model(table = "recency_events", pk = RanjIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct RecencyEvent {
 pub kind: String,
}

fn _injected_id_is_ranjid_desc(e: &RecencyEvent) {
 let _: &::djogi::types::RanjIdDesc = &e.id;
 let _: &::djogi::types::RanjIdRecencyBiased = &e.id;
}

fn main() {}
