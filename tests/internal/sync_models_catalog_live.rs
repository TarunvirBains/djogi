#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]
#![allow(dead_code)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): pg_catalog probes for sync_models DDL — table/column/index/FK rows plus JSONB and PostGIS type metadata.
mod sync_models_catalog_live {
    include!("sources/sync_models_catalog_live.rs");
}
