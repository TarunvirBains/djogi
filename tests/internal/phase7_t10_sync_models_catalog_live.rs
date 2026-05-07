#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]
#![allow(dead_code)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): internal pg_catalog assertions for sync_models DDL shape, distinct from ordinary typed behavior tests.
mod phase7_t10_sync_models_catalog_live {
    include!("sources/phase7_t10_sync_models_catalog_live.rs");
}
