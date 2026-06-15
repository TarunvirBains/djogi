use djogi::prelude::*;

#[model(table = "posts")]
#[derive(Debug, Clone)]
pub struct Post {
 #[field(index = "btre")] // typo
 pub name: String,
}

fn main() {}
