#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): pool-backed on_commit audit-warn probe; needs raw_pool() to discriminate pool vs transaction backing.
mod phase8_on_commit_pool_warn {
    include!("sources/phase8_on_commit_pool_warn.rs");
}
