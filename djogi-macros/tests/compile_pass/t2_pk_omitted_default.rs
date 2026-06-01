// the default-flip regression witness. A model that
// omits `pk` entirely must receive `HeerIdDesc` (== `HeerIdRecencyBiased`)
// as its primary-key type, NOT the pre-flip `HeerId`. Type-level check: if
// someone reverts the default branch in
// `djogi-macros::model::attrs::PkStrategy::default()` back to ascending
// `HeerId`, the `let _: &HeerIdDesc = &p.id;` binding stops compiling.
use djogi::prelude::*;

#[model(table = "flip_probes")]
#[derive(Debug, Clone)]
pub struct FlipProbe {
    pub kind: String,
}

fn _injected_id_is_heerid_desc_when_pk_is_omitted(p: &FlipProbe) {
    let _: &::djogi::types::HeerIdDesc = &p.id;
    let _: &::djogi::types::HeerIdRecencyBiased = &p.id;
}

fn main() {}
