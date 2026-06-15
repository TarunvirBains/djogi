//! `expose(notascope)` must be rejected — only built-in scopes plus the
//! `none` / `internal` sentinels are the accepted built-in scopes.
use djogi::prelude::*;

#[model(table = "users_expose_unknown")]
#[derive(Debug, Clone)]
pub struct User {
 #[field(expose(notascope))]
 pub name: String,
}

fn main() {}
