#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): #37 regression — ForeignKey-typed tenant_key RLS; needs SET LOCAL ROLE + cluster role + hand-emitted policy DDL.
mod t111_rls_fk_tenant_key_live {
    include!("sources/t111_rls_fk_tenant_key_live.rs");
}
