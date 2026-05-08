#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]
#![allow(dead_code)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): apply_plan/bootstrap_ledger probe — asserts djogi_schema_migrations row state and pg_advisory_lock semantics.
mod phase7_t4_runner_live {
    include!("sources/phase7_t4_runner_live.rs");
}
