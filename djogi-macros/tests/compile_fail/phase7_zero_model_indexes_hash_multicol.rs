// Phase 7-Zero v3 T3 — §5 rejection: `using = "hash"` + multi-column.
use djogi::prelude::*;

#[model(table = "users", indexes(
    index(fields = [first_name, last_name], using = "hash"),
))]
#[derive(Debug, Clone)]
pub struct User {
    pub first_name: String,
    pub last_name: String,
}

fn main() {}
