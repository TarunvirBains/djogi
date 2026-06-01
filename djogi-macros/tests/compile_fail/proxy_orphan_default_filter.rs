// Compile-fail fixture: `default_filter = |f| ...`
// without `proxy_for = ParentType` on the same model.
//
// `default_filter` and `default_order` only make sense on proxy
// models — non-proxy models own their own storage and use explicit
// `.filter(...)` / `.order_by(...)` calls. The cross-attribute
// guard in `attrs.rs` surfaces a span-precise diagnostic pointing at
// the offending `default_filter` key (T3.3 VERIFY-1 fixup; was
// `Span::call_site()`).

use djogi::prelude::*;

#[model(
    table = "phase8_proxy_orphan_filter_widgets",
    default_filter = |f| f.active.eq(true),
)]
#[derive(Debug, Clone)]
pub struct Widget {
    pub name: String,
    pub active: bool,
}

fn main() {}
