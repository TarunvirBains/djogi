#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): pg_catalog byte-shape parity probe between sync_models and apply_plan execute paths.
mod sync_models_parity {
    include!("sources/sync_models_parity.rs");
}
