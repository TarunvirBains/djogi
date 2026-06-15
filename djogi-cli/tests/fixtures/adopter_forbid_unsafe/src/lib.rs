//! #370 T-FORBID-UNSAFE: the adopter glue with `#[derive(Model)]` +
//! `link_anchor!()` compiles under `#![forbid(unsafe_code)]` (G6) — all
//! unsafe stays inside the `inventory` crate.
#![forbid(unsafe_code)]

use djogi::prelude::*;

// Exercise link_anchor! (branch b) under forbid-unsafe too.
djogi::link_anchor!();

#[derive(Model)]
#[model(table = "forbid_unsafe_things")]
pub struct Thing {
 pub name: String,
}
