// `#[field(generated = "")]` is rejected at parse
// time.
//
// An empty SQL expression is meaningless — Postgres would reject it on
// `GENERATED ALWAYS AS () STORED` regardless. Catching it at the
// attribute parse stage gives the user a span-precise diagnostic that
// underlines the empty literal rather than failing later at DDL emit.
use djogi::prelude::*;

#[model(table = "users", no_default)]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
    #[field(generated = "")]
    pub email_lower: String,
}

fn main() {}
