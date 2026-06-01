#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]
#![allow(dead_code)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): provisions a virgin DB bypassing setup_test_db_with_extensions to prove db reset replay wires RunnerCtx::audit_pool from ResetRequest (issue #118).
mod c2_118_reset_writes_audit_rows {
    include!("sources/c2_118_reset_writes_audit_rows.rs");
}
