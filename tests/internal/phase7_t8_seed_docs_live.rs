#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]
#![allow(dead_code)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): run_seeds probe — applies SQL fixtures and reads djogi_seed_runs ledger via string_agg; needs current_database().
mod phase7_t8_seed_docs_live {
    include!("sources/phase7_t8_seed_docs_live.rs");
}
