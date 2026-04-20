//! `expose()` with no arguments must be rejected — the grammar requires
//! at least one scope or a sentinel (`none` / `internal`).
use djogi::prelude::*;

#[model(table = "users_expose_empty")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose())]
    pub name: String,
}

fn main() {}
