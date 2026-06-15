// Two `#[field(version)]` annotations on the same model must be rejected.
// The error fires at the second annotated field.
use djogi::prelude::*;

#[model(table = "posts")]
#[derive(Debug, Clone)]
pub struct Post {
 pub title: String,
 #[field(version)]
 pub revision: i32,
 #[field(version)]
 pub version2: i64,
}

fn main() {}
