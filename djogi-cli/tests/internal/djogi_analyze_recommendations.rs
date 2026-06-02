#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): internal CLI/catalog test for Djogi-owned SQL behavior; raw access is outside the ordinary adopter test surface.
mod djogi_analyze_recommendations {
    include!("sources/djogi_analyze_recommendations.rs");
}
