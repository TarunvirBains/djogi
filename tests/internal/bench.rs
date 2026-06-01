#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]
#![cfg(feature = "spatial")]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): smoke bench for convex_hull aggregate + qualify() lowering vs hand-written derived-table baseline as raw SQL.
mod bench {
    include!("sources/bench.rs");
}
