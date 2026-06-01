// Cluster 4 djogi#220 — `#[field(type_change_using = "   ")]`
// is rejected.
//
// Whitespace-only is structurally equivalent to empty — the parser
// rejects both with the same diagnostic. Mirrors the `check` whitespace
// fixture (djogi#105) which guards the same shape.

use djogi::prelude::*;

#[model(table = "items_220_ws", pk = HeerId, no_default)]
#[derive(Debug, Clone)]
pub struct Item220Whitespace {
    #[field(type_change_using = "   \t  ")]
    pub kind: String,
}

fn main() {}
