#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]
#![allow(dead_code)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): out-of-order policy + attune probe — pg_advisory_lock per-bucket isolation and out_of_order_flag bookkeeping.
mod policy_attune_live {
    include!("sources/policy_attune_live.rs");
}
