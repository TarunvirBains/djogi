// bare-identifier `pk = X` grammar. The string-literal
// `pk = "heerid_desc"` form is gone; the attribute parser accepts only bare
// identifiers. `HeerIdRecencyBiased` is the public, adopter-facing alias for
// the internal `HeerIdDesc` strategy and lowers to the same injected type +
// descriptor shape at parse time.
use djogi::prelude::*;

#[model(table = "events", pk = HeerIdRecencyBiased)]
#[derive(Debug, Clone)]
pub struct Event {
    pub kind: String,
}

fn _injected_id_is_heerid_desc(e: &Event) {
    // `HeerIdRecencyBiased` is re-exported from `djogi::types` as an alias
    // for `HeerIdDesc` — confirming the lowering went through.
    let _: &::djogi::types::HeerIdDesc = &e.id;
    let _: &::djogi::types::HeerIdRecencyBiased = &e.id;
    let _: &DateTime = &e.created_at;
    let _: &DateTime = &e.updated_at;
}

fn main() {}
