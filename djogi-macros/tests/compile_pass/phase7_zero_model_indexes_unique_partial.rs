// Phase 7-Zero v3 T3 — §5 positive case: partial unique (forces UniqueIndex).
use djogi::prelude::*;

#[model(table = "accounts", indexes(
    unique(fields = [email], where = "deleted_at IS NULL"),
))]
#[derive(Debug, Clone)]
pub struct Account {
    pub email: String,
    pub deleted_at: Option<DateTime>,
}

fn main() {}
