// §5 rejection: unknown body key inside index(...).
use djogi::prelude::*;

#[model(table = "users", indexes(
 index(fields = [email], wrongkey = "x"),
))]
#[derive(Debug, Clone)]
pub struct User {
 pub email: String,
}

fn main() {}
