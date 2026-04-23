// Phase 7-Zero v3 T3 — §5 positive case: expression index.
use djogi::prelude::*;

#[model(table = "users", indexes(
    index(expr = "lower(email)"),
))]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
}

fn main() {}
