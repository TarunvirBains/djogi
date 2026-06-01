#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): internal PostGIS scalar-expression probe; no ordinary typed scalar terminal exists for these expressions.
mod zero_cluster_c_spatial_scalar_live {
    include!("sources/zero_cluster_c_spatial_scalar_live.rs");
}
