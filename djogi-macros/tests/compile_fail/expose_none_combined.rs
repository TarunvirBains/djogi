//! `expose(none, public)` — `none` / `internal` sentinels cannot be
//! combined with any other scope. Must be rejected.
use djogi::prelude::*;

#[model(table = "users_expose_none_combined")]
#[derive(Debug, Clone)]
pub struct User {
    #[field(expose(none, public))]
    pub name: String,
}

fn main() {}
