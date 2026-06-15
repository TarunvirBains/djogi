// §5 Q5 rejection: opclass fails ASCII shape check.
//
// Q5 mandates a byte-level ASCII ident rule: first byte is `_` or an
// ASCII letter, remaining bytes are `_` or ASCII alphanumerics,
// total ≤ 63 bytes. `"1bad"` starts with a digit, which is not an
// accepted first byte.
use djogi::prelude::*;

#[model(table = "users", indexes(
 index(fields = [email], using = "gin", opclass = "1bad"),
))]
#[derive(Debug, Clone)]
pub struct User {
 pub email: String,
}

fn main() {}
