// `#[field(version)]` must reject multi-segment paths that are NOT the
// explicit `std::primitive::i32` / `core::primitive::i32` allowlist, even
// when the final segment is `i32`. A user module path like `my_mod::i32`
// does not prove the field is genuinely a primitive — it could be any
// user-defined type alias. The validator rejects it at macro-expansion
// time so misleading names cannot silently satisfy the contract.
use djogi::prelude::*;

mod my_mod {
 // Deliberately NOT i32 — proves the validator is checking the path
 // shape, not the resolved type.
 #[allow(non_camel_case_types)]
 pub type i32 = String;
}

#[model(table = "posts")]
#[derive(Debug, Clone)]
pub struct Post {
 pub title: String,
 #[field(version)]
 pub revision: my_mod::i32,
}

fn main() {}
