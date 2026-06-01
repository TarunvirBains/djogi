#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]
#![allow(dead_code)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): PK-type-flip runner probe — reads information_schema and pg_trigger to verify shadow-column install and cutover.
mod t9_pk_flip_live {
    include!("sources/t9_pk_flip_live.rs");
}
