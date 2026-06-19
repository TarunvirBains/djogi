// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

//! `cargo-djogi` executable — thin shim over
//! [`djogi_cli::run_cargo_wrapper_from_env`]. All wrapper logic lives in
//! the `djogi_cli` library crate. This binary target exists solely so
//! Cargo can build the `cargo-djogi` executable that enables the
//! `cargo djogi <subcommand>` developer invocation.

fn main() -> std::process::ExitCode {
    djogi_cli::run_cargo_wrapper_from_env()
}
