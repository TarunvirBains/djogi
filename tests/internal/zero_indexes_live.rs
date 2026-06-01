#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]
#![allow(dead_code)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): IndexSpec → DDL round-trip via test-local renderer (pre-emitter); probes pg_index/pg_constraint/pg_am.
mod zero_indexes_live {
    include!("sources/zero_indexes_live.rs");
}
