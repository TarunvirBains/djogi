// Phase 7-Zero v3 T3 — §5 rejection: unknown top-level entry inside
// `indexes(...)` (only `index(...)` and `unique(...)` are accepted).
use djogi::prelude::*;

#[model(table = "users", indexes(
    bogus(fields = [email]),
))]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
}

fn main() {}
