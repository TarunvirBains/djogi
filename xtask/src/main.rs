mod check_justifications;
mod check_spatial_ci;
mod check_test_surface;
mod gc_target_cache;

use std::process::ExitCode;

const USAGE: &str = "usage: cargo xtask check-justifications | check-spatial-ci | check-test-surface [--list] | gc-target-cache [--dry-run]";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);

    match (args.next().as_deref(), args.next().as_deref(), args.next()) {
        (Some("check-justifications"), None, None) => check_justifications::run(),
        (Some("check-spatial-ci"), None, None) => check_spatial_ci::run(),
        (Some("check-test-surface"), None, None) => check_test_surface::run(false),
        (Some("check-test-surface"), Some("--list"), None) => check_test_surface::run(true),
        (Some("gc-target-cache"), None, None) => gc_target_cache::run(false),
        (Some("gc-target-cache"), Some("--dry-run"), None) => gc_target_cache::run(true),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}
