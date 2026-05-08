#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): DjogiPool smoke bench — max_size concurrency, post_connect cost, with_client acquire/release ratio via SELECT 1.
mod phase8_zero_pool_bench {
    include!("sources/phase8_zero_pool_bench.rs");
}
