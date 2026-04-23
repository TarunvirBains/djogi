// Phase 7-Zero v3 T3 — §5 positive case: concurrent build flag (Q1).
//
// `concurrently = true` at declaration sets
// `IndexSpec::requires_out_of_transaction = true`; Phase 7's migration
// runner emits `CREATE INDEX CONCURRENTLY` in every profile.
use djogi::prelude::*;

#[model(table = "users_email", indexes(
    index(fields = [email], concurrently = true),
))]
#[derive(Debug, Clone)]
pub struct UserEmail {
    pub email: String,
}

fn main() {}
