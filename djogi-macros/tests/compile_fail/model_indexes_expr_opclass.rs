// §4 pass-2 P1-02 rejection: expression indexes
// do not accept opclass in 0.1.0.
use djogi::prelude::*;

#[model(table = "users", indexes(
    index(expr = "lower(email)", opclass = "text_pattern_ops"),
))]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
}

fn main() {}
