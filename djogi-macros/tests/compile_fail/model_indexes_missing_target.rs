// §5 rejection: neither `fields` nor `expr`.
use djogi::prelude::*;

#[model(table = "users", indexes(
 index(using = "btree"),
))]
#[derive(Debug, Clone)]
pub struct User {
 pub email: String,
}

fn main() {}
