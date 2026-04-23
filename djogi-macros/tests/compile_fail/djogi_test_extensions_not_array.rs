//! `#[djogi_test(extensions = "postgis")]` — the value must be an array
//! literal. A bare string is a common authoring mistake and the macro should
//! point the user at the correct shape.

#[djogi::djogi_test(extensions = "postgis")]
async fn extensions_must_be_array(mut _ctx: djogi::DjogiContext) {}

fn main() {}
