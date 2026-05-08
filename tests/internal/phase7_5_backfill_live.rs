#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): live_migrate::execute_backfill/resume_backfill chunk-loop probe; seeds via generate_series and asserts NULL frontier.
mod phase7_5_backfill_live {
    include!("sources/phase7_5_backfill_live.rs");
}
