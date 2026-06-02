//! Djogi CLI binary — thin shim over [`djogi_cli::run_from_env`].
//! All CLI logic lives in the `djogi_cli` library crate. The binary
//! target exists solely so Cargo can build the `djogi` executable.

fn main() -> std::process::ExitCode {
    djogi_cli::run_from_env()
}
