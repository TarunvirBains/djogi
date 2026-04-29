// `default_volatility` is meaningless without a
// `default = "..."` attribute on the same field; the override
// classifies a default expression that does not exist. The macro
// rejects rather than silently accepting a no-op annotation.
use djogi::prelude::*;

#[model(table = "events")]
#[derive(Debug, Clone)]
pub struct Event {
    #[field(default_volatility = "stable")]
    pub fired_at: DateTime,
}

fn main() {}
