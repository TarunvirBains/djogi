// §5 Q5 rejection: per-column opclass fails
// the ASCII shape check. Same byte-level rule as the top-level
// `opclass` path; the opclass validator walks every IndexColumnSpec
// in the record form.
use djogi::prelude::*;

#[model(table = "users", indexes(
    index(fields = [(col = email, opclass = "9starts_with_digit")]),
))]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
}

fn main() {}
