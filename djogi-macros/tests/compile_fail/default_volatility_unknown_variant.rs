// `default_volatility` accepts exactly three
// variants: `"immutable"`, `"stable"`, `"volatile"`. Anything else
// is rejected with an error that lists every valid choice so
// adopters can fix the typo without consulting the docs.
use djogi::prelude::*;

#[model(table = "events")]
#[derive(Debug, Clone)]
pub struct Event {
 #[field(default = "now()", default_volatility = "wibble")]
 pub fired_at: DateTime,
}

fn main() {}
