// bare `#[field(unique)]` on a scalar column compiles.
//
// Plain uniqueness declarations remain simple field-level shorthand. The
// T2 validations only reject *incompatible* combinations
// (hash+unique, gin-on-scalar, nulls_not_distinct at the field level);
// the simple-unique case stays a one-liner.
use djogi::prelude::*;

#[model(table = "users")]
#[derive(Debug, Clone)]
pub struct User {
 #[field(unique)]
 pub email: String,
}

fn main() {}
