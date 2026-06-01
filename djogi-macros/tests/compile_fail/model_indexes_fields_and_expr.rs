// §5 rejection: `fields` and `expr` mutually exclusive.
use djogi::prelude::*;

#[model(table = "users", indexes(
    index(fields = [email], expr = "lower(email)"),
))]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
}

fn main() {}
