// `#[field(max_length = 0)]` must be rejected at macro-expansion time.
// Postgres refuses `VARCHAR(0)` with "length for type varchar must be at least 1";
// the macro catches this early so the error is span-precise and references the
// offending literal rather than surfacing as a runtime migration failure.
use djogi::prelude::*;

#[model(table = "posts")]
#[derive(Debug, Clone)]
pub struct Post {
 #[field(max_length = 0)]
 pub title: String,
}

fn main() {}
