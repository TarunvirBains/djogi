#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): round-trips lower_delta-emitted EXCLUDE/GENERATED DDL via pg_constraint.contype and pg_attribute.attgenerated.
mod pr7_exclusion_generated_live {
    include!("sources/pr7_exclusion_generated_live.rs");
}
