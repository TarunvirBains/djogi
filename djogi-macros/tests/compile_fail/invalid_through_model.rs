use djogi::prelude::*;

#[model(table = "bad_through", through)]
#[derive(Debug, Clone)]
pub struct BadThrough {
 pub label: String,
}

fn main() {}
