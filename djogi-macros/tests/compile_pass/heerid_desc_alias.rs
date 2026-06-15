// the two recency-biased identifiers
// (`HeerIdRecencyBiased` + `HeerIdDesc`) compile to the same injected
// `id: HeerIdDesc` field. Both spellings are supported so callers reading
// migration internals (where the descending type leaks through) don't
// need to remember a second name. The attribute-level grammar collapses
// them to a single internal `PkStrategy::HeerIdDesc` variant, so there's
// only one code path to test.
use djogi::prelude::*;

#[model(table = "recency_a", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct RecencyA {
 pub data: String,
}

#[model(table = "internal_b", pk = HeerIdDesc)]
#[derive(Debug, Clone)]
pub struct InternalB {
 pub data: String,
}

fn _both_inject_heerid_desc(a: &RecencyA, b: &InternalB) {
 let _: &::djogi::types::HeerIdDesc = &a.id;
 let _: &::djogi::types::HeerIdDesc = &b.id;
}

fn main() {}
