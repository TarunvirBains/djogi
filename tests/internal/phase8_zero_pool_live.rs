#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): DjogiPool deadpool lifecycle — post_connect single-fire, max_size saturation timeout, Status::size on Ok/Err/panic, and raw_with_client COPY.
mod phase8_zero_pool_live {
    include!("sources/phase8_zero_pool_live.rs");
}
