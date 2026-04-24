// Phase 7-Zero-2 T2 — the pre-T2 string-literal `pk = "…"` grammar is
// removed. The parser must produce a span-carrying diagnostic directing
// callers at the bare-identifier replacement rather than the generic
// "expected key = value attribute" catch-all.
use djogi::prelude::*;

#[model(table = "foo", pk = "heerid")]
pub struct Foo {
    pub bar: String,
}

fn main() {}
