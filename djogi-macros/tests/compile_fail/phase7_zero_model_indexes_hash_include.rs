// Phase 7-Zero v3 T3 — §5 rejection: `using = "hash"` + `include`.
use djogi::prelude::*;

#[model(table = "users", indexes(
    index(fields = [email], using = "hash", include = [status]),
))]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
    pub status: String,
}

fn main() {}
