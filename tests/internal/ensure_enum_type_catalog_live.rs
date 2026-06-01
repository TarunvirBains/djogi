#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): internal catalog probe for ensure_enum_type label persistence and verbatim storage.
mod ensure_enum_type_catalog_live {
    include!("sources/ensure_enum_type_catalog_live.rs");
}
