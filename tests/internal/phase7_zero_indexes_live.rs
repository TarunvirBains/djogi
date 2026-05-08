#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]
#![allow(dead_code)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): IndexSpec → DDL round-trip via test-local renderer (pre-Phase 7 emitter); probes pg_index/pg_constraint/pg_am.
mod phase7_zero_indexes_live {
    include!("sources/phase7_zero_indexes_live.rs");
}
