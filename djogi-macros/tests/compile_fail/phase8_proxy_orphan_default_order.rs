// Phase 8β T3.5 — Compile-fail fixture: `default_order = [...]`
// without `proxy_for = ParentType` on the same model.
//
// Same rationale as `phase8_proxy_orphan_default_filter.rs` —
// `default_order` is only meaningful on proxy models. The diagnostic
// span points at the offending `default_order` key per T3.3's
// VERIFY-1 fixup.

use djogi::prelude::*;

#[model(
    table = "phase8_proxy_orphan_order_widgets",
    default_order = [(name, Asc)],
)]
#[derive(Debug, Clone)]
pub struct Widget {
    pub name: String,
    pub active: bool,
}

fn main() {}
