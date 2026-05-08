#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]
#![allow(dead_code)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): rollback/fake_apply/baseline/verify/repair runner probe — asserts djogi_schema_migrations status transitions.
mod phase7_t5_repair_verify_live {
    include!("sources/phase7_t5_repair_verify_live.rs");
}
