#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#162): pool-path raw_execute must detach the connection on Err / cancellation; pin the dirty-by-default lifecycle.
mod phase8_5_c2_162_raw_dirty_semantics {
    include!("sources/phase8_5_c2_162_raw_dirty_semantics.rs");
}
