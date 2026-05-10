mod check_justifications;
mod check_spatial_ci;
mod check_test_surface;

use std::process::ExitCode;

const USAGE: &str =
    "usage: cargo xtask check-justifications | check-spatial-ci | check-test-surface [--list]";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);

    match (args.next().as_deref(), args.next().as_deref(), args.next()) {
        (Some("check-justifications"), None, None) => check_justifications::run(),
        (Some("check-spatial-ci"), None, None) => check_spatial_ci::run(),
        (Some("check-test-surface"), None, None) => check_test_surface::run(false),
        (Some("check-test-surface"), Some("--list"), None) => check_test_surface::run(true),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}
