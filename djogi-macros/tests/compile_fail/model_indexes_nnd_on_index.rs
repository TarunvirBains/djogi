// §5 rejection: `nulls_not_distinct` on `index(...)`.
use djogi::prelude::*;

#[model(table = "users", indexes(
 index(fields = [email], nulls_not_distinct = true),
))]
#[derive(Debug, Clone)]
pub struct User {
 pub email: String,
}

fn main() {}
