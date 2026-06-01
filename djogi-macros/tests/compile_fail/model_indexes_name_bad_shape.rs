// §5 rejection: explicit `name` fails the same
// 63-byte + ASCII-ident shape check as generated names. `"1-bad"`
// starts with a digit AND contains a hyphen — both non-accepted.
use djogi::prelude::*;

#[model(table = "users", indexes(
    index(fields = [email], name = "1-bad"),
))]
#[derive(Debug, Clone)]
pub struct User {
    pub email: String,
}

fn main() {}
