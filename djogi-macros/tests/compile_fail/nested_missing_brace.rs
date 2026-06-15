//! nested traversal without enclosing braces is a
//! parse error. After a peer path the parser expects either a `,` /
//! end-of-list (terminal) or a `{` (start of nested block). A bare
//! identifier following the peer path is rejected with a span-carrying
//! diagnostic at the unexpected token.
use djogi::prelude::*;

#[model(table = "deps_t6_missing_brace")]
#[derive(Debug, Clone)]
pub struct Dep {
 #[field(expose(public))]
 pub name: String,
}

#[model(table = "emps_t6_missing_brace", no_default)]
#[derive(Debug, Clone)]
pub struct Emp {
 // Nested arrow without enclosing `{... }` — the `manager_id` token
 // after the peer path is not a recognised continuation; the parser
 // expects either end-of-list or a brace group.
 #[field(expose(public -> Dep manager_id -> DepPublic))]
 pub dept: ForeignKey<Dep>,
}

fn main() {}
