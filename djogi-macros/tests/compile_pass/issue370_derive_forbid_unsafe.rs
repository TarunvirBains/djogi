//! #370 T-FORBID-UNSAFE (derive portion): #[derive(Model)]'s inventory
//! machinery contains no `unsafe` tokens of its own (all unsafe is
//! encapsulated inside the `inventory` crate). Compiles under
//! `#![forbid(unsafe_code)]` at the crate root.
#![forbid(unsafe_code)]

use djogi::prelude::*;

#[model(table = "forbid_unsafe_rows")]
pub struct ForbidUnsafeRow {
    name: String,
}

fn main() {}
