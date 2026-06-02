#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): set_role transaction-scoping probe — CREATE ROLE in DO-block + current_user readback to pin SET LOCAL ROLE scope.
mod set_role_transaction_scoped {
    include!("sources/set_role_transaction_scoped.rs");
}
