// Phase 7-Zero v3 T3 — §5 rejection: column name not declared on struct.
use djogi::prelude::*;

#[model(table = "users", indexes(
    index(fields = [nonexistent_column]),
))]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
}

fn main() {}
