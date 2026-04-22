use djogi::prelude::*;

#[model(table = "posts")]
#[derive(Debug, Clone)]
pub struct Post {
    #[field(index = 42)]
    pub name: String,
}

fn main() {}
