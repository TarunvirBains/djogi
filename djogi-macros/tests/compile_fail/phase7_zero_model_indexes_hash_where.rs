// Phase 7-Zero v3 T3 — §5 rejection: `using = "hash"` + `where`.
use djogi::prelude::*;

#[model(table = "users", indexes(
    index(fields = [email], using = "hash", where = "deleted_at IS NULL"),
))]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
    pub deleted_at: Option<DateTime>,
}

fn main() {}
