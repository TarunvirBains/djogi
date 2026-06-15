// the nested-brace form
// `expose(scope -> Peer { field -> Peer2 })` is parseable (the
// structural types are in place) but the visage emitter does not yet
// consume it. T6 rejects it at parse time with an actionable error
// rather than silently dropping the nested traversal. The well-formed
// non-nested `-> Peer` form still compiles (see
// `phase7_zero2_t6_arrow_narrow.rs` and `phase7_zero2_t6_arrow_full_peer.rs`).

use djogi::prelude::*;

#[model(table = "deps")]
#[derive(Debug, Clone)]
pub struct Dep {
 #[field(expose(public))]
 pub name: String,
}

#[model(table = "emps")]
#[derive(Debug, Clone)]
pub struct Emp {
 #[field(expose(public -> Dep { name -> DepPublic }))]
 pub dept: ForeignKey<Dep>,
}

fn main() {}
