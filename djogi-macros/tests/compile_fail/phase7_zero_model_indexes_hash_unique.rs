// Phase 7-Zero v3 T3 — §5 rejection: `using = "hash"` + unique.
use djogi::prelude::*;

#[model(table = "users", indexes(
    unique(fields = [email], using = "hash"),
))]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
}

fn main() {}
