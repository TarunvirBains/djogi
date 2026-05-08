#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#133): spawns the djogi binary as a subprocess and reads current_database() to splice into a fixture Djogi.toml.
mod phase8_djogi_verify_cli {
    include!("sources/phase8_djogi_verify_cli.rs");
}
