#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): D7 set_tenant/set_role ordering probe — reads current_user and the app.tenant_id GUC under a DO-block role.
mod phase8_set_role_set_tenant_ordering {
    include!("sources/phase8_set_role_set_tenant_ordering.rs");
}
