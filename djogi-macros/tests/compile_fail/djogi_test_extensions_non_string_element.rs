//! `#[djogi_test(extensions = [42])]` — every array element must be a
//! string literal. A non-string element should fail with a span pointing
//! at the offending token.

#[djogi::djogi_test(extensions = [42])]
async fn extensions_elements_must_be_strings(mut _ctx: djogi::DjogiContext) {}

fn main() {}
