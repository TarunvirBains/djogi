// §5 rejection: `using = "hash"` + `expr`.
use djogi::prelude::*;

#[model(table = "users", indexes(
 index(expr = "lower(email)", using = "hash"),
))]
#[derive(Debug, Clone)]
pub struct User {
 pub email: String,
}

fn main() {}
