// `#[field(version)]` on an `Option<i32>` field must be rejected with a
// span-precise compile error. The type check inspects the last path segment:
// `Option<i32>` has last segment `Option`, not `i32` — correctly rejected.
use djogi::prelude::*;

#[model(table = "posts")]
#[derive(Debug, Clone)]
pub struct Post {
 pub title: String,
 #[field(version)]
 pub revision: Option<i32>,
}

fn main() {}
