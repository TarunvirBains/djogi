//! Cargo subcommand wrapper for adopters.
//!
//! `cargo djogi ...` resolves the adopter-linked workspace binary from
//! `Djogi.toml`'s `[cli]` table, builds it when needed, and forwards
//! all subsequent CLI arguments.

use djogi::config::DjogiConfig;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};

fn main() -> ExitCode {
    match run() {
        Ok(code) => exit_code(code),
        Err(err) => {
            eprintln!("cargo djogi: {err}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<i32, String> {
    let workspace = locate_workspace_root()?;
    let config = DjogiConfig::load_from_workspace(&workspace).map_err(|err| {
        format!(
            "failed to load Djogi.toml from {}: {err}",
            workspace.join("Djogi.toml").display()
        )
    })?;

    let package = if config.cli.package.trim().is_empty() {
        return Err("Djogi.toml is missing [cli].package".to_string());
    } else {
        config.cli.package.trim().to_string()
    };
    let bin = if config.cli.bin.trim().is_empty() {
        "djogi".to_string()
    } else {
        config.cli.bin.trim().to_string()
    };

    let target_dir = workspace_target_dir(&workspace);
    build_adopter_binary(&workspace, &target_dir, &package, &bin)?;
    let binary = binary_path(&target_dir, &bin);
    if !binary.exists() {
        return Err(format!(
            "built {bin} binary missing at {}",
            binary.display()
        ));
    }

    run_adopter_binary(&binary)
}

fn locate_workspace_root() -> Result<PathBuf, String> {
    let mut current = std::env::current_dir().map_err(|err| err.to_string())?;
    loop {
        if current.join("Djogi.toml").is_file() && current.join("Cargo.toml").is_file() {
            return Ok(current);
        }
        if !current.pop() {
            break;
        }
    }
    Err("could not locate workspace root from current directory".to_string())
}

fn workspace_target_dir(workspace: &Path) -> PathBuf {
    if let Ok(target_dir) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(target_dir);
    }
    workspace.join("target")
}

fn build_adopter_binary(
    workspace: &Path,
    target_dir: &Path,
    package: &str,
    bin: &str,
) -> Result<(), String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(&cargo)
        .arg("build")
        .arg("--locked")
        .arg("--package")
        .arg(package)
        .arg("--bin")
        .arg(bin)
        .arg("--manifest-path")
        .arg(workspace.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(target_dir)
        .status()
        .map_err(|err| format!("failed to run cargo build: {err}"))?;

    if !status.success() {
        return Err(format!(
            "cargo build failed for package '{package}', bin '{bin}' with status {status}"
        ));
    }
    Ok(())
}

fn binary_path(target_dir: &Path, bin: &str) -> PathBuf {
    if cfg!(windows) {
        target_dir.join("debug").join(format!("{bin}.exe"))
    } else {
        target_dir.join("debug").join(bin)
    }
}

fn run_adopter_binary(binary: &Path) -> Result<i32, String> {
    let status: ExitStatus = Command::new(binary)
        .args(std::env::args_os().skip(1))
        .current_dir(std::env::current_dir().map_err(|err| err.to_string())?)
        .status()
        .map_err(|err| format!("failed to run {}: {err}", binary.display()))?;

    status
        .code()
        .ok_or_else(|| format!("{} terminated by signal", binary.display()))
}

fn exit_code(code: i32) -> ExitCode {
    u8::try_from(code).map_or_else(|_| ExitCode::from(1), ExitCode::from)
}
