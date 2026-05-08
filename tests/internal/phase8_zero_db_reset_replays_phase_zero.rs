#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]
#![allow(dead_code)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): provisions a virgin DB bypassing setup_test_db_with_extensions to prove db reset replays the Phase 0 bootstrap.
mod phase8_zero_db_reset_replays_phase_zero {
    include!("sources/phase8_zero_db_reset_replays_phase_zero.rs");
}
