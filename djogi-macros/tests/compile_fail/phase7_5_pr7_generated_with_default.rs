// Phase 7.5 PR 7 — `#[field(generated = "...")]` and `#[field(default
// = "...")]` are mutually exclusive.
//
// Postgres rejects a column declaration that carries both a DEFAULT
// clause and a `GENERATED ALWAYS AS (...) STORED` clause — the
// generated expression is the value source. We catch the conflict at
// macro time so the operator sees the rule before any DDL is emitted,
// rather than at apply time.
use djogi::prelude::*;

#[model(table = "users", no_default)]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
    #[field(generated = "LOWER(email)", default = "''")]
    pub email_lower: String,
}

fn main() {}
