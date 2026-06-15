//! Cargo subcommand wrapper for adopters.
//!
//! `cargo djogi...` resolves the adopter-linked workspace binary from
//! `Djogi.toml`'s `[cli]` table, builds it when needed, and forwards
//! all subsequent CLI arguments.

use djogi::config::DjogiConfig;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

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
    let binary = discover_binary_path(&workspace, &target_dir, &package, &bin)?;

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

/// Build the adopter binary and discover its actual path from Cargo's
/// `--message-format=json` output, instead of assuming a hardcoded
/// `target/debug/<bin>` layout. This handles cross-compilation targets,
/// custom target dirs, and any future Cargo layout changes.
fn discover_binary_path(
    workspace: &Path,
    target_dir: &Path,
    package: &str,
    bin: &str,
) -> Result<PathBuf, String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(&cargo)
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
        .arg("--message-format")
        .arg("json")
        .output()
        .map_err(|err| format!("failed to run cargo build: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "cargo build failed for package '{package}', bin '{bin}': {stderr}"
        ));
    }

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let msg: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if msg.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }
        let target_kind: Vec<&str> = msg
            .get("target")
            .and_then(|t| t.get("kind"))
            .and_then(|k| k.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        if !target_kind.contains(&"bin") {
            continue;
        }
        let bin_name = match msg
            .get("target")
            .and_then(|t| t.get("name"))
            .and_then(|n| n.as_str())
        {
            Some(name) => name,
            None => continue,
        };
        if bin_name != bin {
            continue;
        }
        if let Some(filenames) = msg.get("filenames").and_then(|f| f.as_array()) {
            for file in filenames.iter().filter_map(|f| f.as_str()) {
                if !file.ends_with(".d") && !file.ends_with(".pdb") {
                    return Ok(PathBuf::from(file));
                }
            }
        }
    }

    Err(format!(
        "cargo build succeeded but no binary artifact found for '{bin}'"
    ))
}

fn run_adopter_binary(binary: &Path) -> Result<i32, String> {
    let status: std::process::ExitStatus = Command::new(binary)
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
