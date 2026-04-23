//! `#[djogi_test(foo = "bar")]` — any key other than `extensions` must be
//! rejected with a span-precise compile error.

#[djogi::djogi_test(foo = "bar")]
async fn unknown_arg_rejected(mut _ctx: djogi::DjogiContext) {}

fn main() {}
