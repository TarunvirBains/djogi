#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): internal RLS role/policy probe requires session role and catalog setup outside ordinary typed tests.
mod zero_tree_query_rls_live {
    include!("sources/zero_tree_query_rls_live.rs");
}
