#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): internal framework test for Djogi-owned SQL/driver behavior; raw access is outside the ordinary adopter test surface.
mod phase7_t10_sync_models_parity {
    include!("sources/phase7_t10_sync_models_parity.rs");
}
