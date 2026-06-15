// `#[field(max_length = N)]` is a `String`-only contract and must be
// rejected for unsupported non-String fields at macro-expansion time.
use djogi::prelude::*;

#[model(table = "wrong_type_max_length", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct WrongTypeMaxLength {
 #[field(max_length = 64)]
 pub view_count: i64,
}

fn main() {}
