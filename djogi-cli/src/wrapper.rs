// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

//! Cargo subcommand wrapper (`cargo djogi …`) for adopters.
//!
//! # What
//! Implements the `cargo-djogi` executable: a developer convenience that
//! lets `cargo djogi <subcommand>` resolve, build, and forward to the
//! adopter-linked `djogi` binary declared in `Djogi.toml`'s `[cli]` table.
//!
//! # Why
//! Adopters' descriptor-aware `djogi` binary must link their model crates
//! (see the `DescriptorProvider` boundary, djogi#370). Typing the full
//! `cargo build … && ./target/…/djogi …` dance by hand is tedious during
//! local development; `cargo djogi …` collapses it to one command.
//!
//! # How
//! [`run_cargo_wrapper_from_env`] loads `Djogi.toml` from the workspace
//! root, runs `cargo build --locked -p <package> --bin <bin>
//! --message-format=json`, parses the produced artifact path from Cargo's
//! JSON stream, then execs that binary forwarding `argv[1..]`.
//!
//! # Where
//! This is a **local development** path. Production and CI MUST run the
//! prebuilt adopter-linked `djogi` binary directly (e.g. `djogi migrations
//! apply` in a migration image), NOT `cargo djogi`, because `cargo djogi`
//! requires a buildable workspace and a Cargo toolchain.

use djogi::config::DjogiConfig;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Resolve the `(package, bin)` pair the wrapper should build, applying the
/// `[cli].bin` default of `"djogi"` and rejecting an empty `[cli].package`.
fn resolve_wrapper_target(config: &DjogiConfig) -> Result<(String, String), String> {
    let package = config.cli.package.trim();
    if package.is_empty() {
        return Err("Djogi.toml is missing [cli].package".to_string());
    }
    let bin = config.cli.bin.trim();
    let bin = if bin.is_empty() { "djogi" } else { bin };
    Ok((package.to_string(), bin.to_string()))
}

/// Parse Cargo `--message-format=json` stdout and return the filesystem
/// path of the compiled binary artifact whose target name equals `bin`.
fn parse_artifact_path(stdout: &str, bin: &str) -> Result<PathBuf, String> {
    for line in stdout.lines() {
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

/// Entry point for the `cargo-djogi` executable. Loads `Djogi.toml`,
/// builds the adopter-linked binary, and forwards all CLI arguments to it.
///
/// Returns the adopter binary's exit code, or `ExitCode::from(1)` on any
/// wrapper-level failure (config load, build failure, artifact not found,
/// or the child being terminated by a signal). Errors are printed to
/// stderr prefixed with `cargo djogi:` before the non-zero code is returned.
///
/// # Stability
/// This is a **binary entrypoint**, not part of `djogi-cli`'s stable adopter
/// API surface. It is `pub` only because `src/bin/cargo-djogi.rs` is a
/// separate crate-compilation-unit and must reach it through the library's
/// public surface (exactly as `main.rs` reaches `run_from_env`). It is marked
/// `#[doc(hidden)]` so it does not appear prominently in generated docs.
/// Adopters should not call it directly.
#[doc(hidden)]
pub fn run_cargo_wrapper_from_env() -> ExitCode {
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

    let (package, bin) = resolve_wrapper_target(&config)?;

    let target_dir = workspace_target_dir(&workspace);
    let binary = build_and_discover_binary(&workspace, &target_dir, &package, &bin)?;

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
fn build_and_discover_binary(
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

    parse_artifact_path(&String::from_utf8_lossy(&output.stdout), bin)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_target_defaults_bin_to_djogi_when_empty() {
        let mut config = DjogiConfig::default();
        config.cli.package = "my-app-bin".to_string();
        config.cli.bin = String::new();
        let (package, bin) = resolve_wrapper_target(&config).expect("should resolve");
        assert_eq!(package, "my-app-bin");
        assert_eq!(bin, "djogi");
    }

    #[test]
    fn resolve_target_rejects_empty_package() {
        let config = DjogiConfig::default();
        let err = resolve_wrapper_target(&config).expect_err("empty package must error");
        assert!(
            err.contains("[cli].package"),
            "error must name the missing config key, got: {err}"
        );
    }

    #[test]
    fn parse_artifact_path_picks_matching_bin_skipping_dot_d() {
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","target":{"name":"other","kind":["lib"]},"filenames":["/x/libother.rlib"]}"#,
            "\n",
            r#"{"reason":"compiler-artifact","target":{"name":"djogi","kind":["bin"]},"filenames":["/x/target/debug/djogi.d","/x/target/debug/djogi"]}"#,
            "\n"
        );
        let path = parse_artifact_path(stdout, "djogi").expect("should find djogi bin");
        assert_eq!(path, PathBuf::from("/x/target/debug/djogi"));
    }

    #[test]
    fn parse_artifact_path_errors_when_no_matching_bin() {
        let stdout = r#"{"reason":"compiler-artifact","target":{"name":"other","kind":["bin"]},"filenames":["/x/target/debug/other"]}"#;
        let err = parse_artifact_path(stdout, "djogi").expect_err("no djogi bin present");
        assert!(
            err.contains("djogi"),
            "error must name the missing binary, got: {err}"
        );
    }
}
