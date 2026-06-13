#![allow(clippy::disallowed_methods)]
#![allow(clippy::module_inception)]

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (djogi#355): spawns the djogi binary as a subprocess and
// probes the per-test database directly to confirm rollback side effects.
mod djogi_migrations_rollback_cli {
    include!("sources/djogi_migrations_rollback_cli.rs");
}
