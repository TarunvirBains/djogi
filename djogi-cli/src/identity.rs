//! CLI node identity resolver.
//!
//! Resolves the runner's [`djogi::migrate::RunnerIdentity`] from the
//! operator's CLI flags and environment, enforcing the precedence
//! and conflict rules:
//!
//! 1. Explicit `--node-id <id>` wins over `HEER_NODE_ID` env var.
//! 2. `--node-id` and `--single-node-dev` are mutually exclusive.
//! 3. Node ID range: `0..=511` (HeerId range).
//! 4. Missing identity for non-dev operations refuses with exit code 2.
//! 5. `--single-node-dev` is refused in production profile or
//!    `DJOGI_ENV=production`.

use std::process::ExitCode;

/// Resolved node identity from CLI flags + environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliResolvedIdentity {
    /// Explicit `--node-id <id>` supplied by the operator.
    Selected(i32),
    /// `--single-node-dev` flag supplied — binds node 1.
    SingleNodeDev,
}

/// Errors that can occur during CLI identity resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliIdentityResolveError {
    /// Both `--node-id` and `--single-node-dev` were supplied.
    ConflictingFlags,
    /// `--node-id` value is outside the valid HeerId range `0..=511`.
    NodeIdOutOfRange { value: i32 },
    /// Non-dev operation requires explicit node identity but none was provided.
    MissingNodeIdentity,
    /// `--single-node-dev` is not permitted in production profile or
    /// when `DJOGI_ENV=production`.
    SingleNodeDevRefusedInProduction,
    /// `HEER_NODE_ID` env var is set but cannot be parsed as a node ID.
    InvalidEnvFormat { value: String },
}

impl std::fmt::Display for CliIdentityResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        match self {
            Self::ConflictingFlags => write!(
                f,
                "--node-id and --single-node-dev are mutually exclusive; \
                 supply only one"
            ),
            Self::NodeIdOutOfRange { value } => write!(
                f,
                "--node-id {} is out of range; must be 0..=511 (HeerId range)",
                value
            ),
            Self::MissingNodeIdentity => write!(
                f,
                "missing node identity for this operation — \
                 supply --node-id <id> or --single-node-dev"
            ),
            Self::SingleNodeDevRefusedInProduction => write!(
                f,
                "--single-node-dev is not permitted in production; \
                 use --node-id with a registered cluster node identity"
            ),
            Self::InvalidEnvFormat { value } => write!(
                f,
                "HEER_NODE_ID value {:?} is not a valid integer; \
                 supply a numeric node ID or unset the variable",
                value
            ),
        }
    }
}

/// Validate that a node ID is within the valid HeerId range.
pub fn validate_node_id_range(node_id: i32) -> bool {
    (0..=511).contains(&node_id)
}

/// Check whether the current environment is considered production.
/// Returns `true` when `DJOGI_ENV` equals `"production"` (case-sensitive).
fn is_production_env() -> bool {
    std::env::var("DJOGI_ENV").as_deref() == Ok("production")
}

/// Resolve the CLI node identity from flags and environment.
///
/// Arguments:
/// - `cli_node_id`: `Some(id)` if `--node-id` was explicitly passed; `None` otherwise.
/// - `single_node_dev`: `true` if `--single-node-dev` was passed.
/// - `profile`: the `Djogi.toml::profile` value (e.g. `"development"`, `"production"`).
/// - `operation_name`: a label for the operation, used in error messages.
///
/// Resolution order:
/// 1. If both `--node-id` and `--single-node-dev` are set → `ConflictingFlags`.
/// 2. If `--node-id` is set, validate range `0..=511` → `Selected(id)`.
/// 3. If `--single-node-dev` is set:
///    - Refuse in production profile or `DJOGI_ENV=production` → `SingleNodeDevRefusedInProduction`.
///    - Otherwise → `SingleNodeDev`.
/// 4. Fall back to `HEER_NODE_ID` env var:
///    - If present and valid → `Selected(parsed)`.
///    - If present but out of range or unparseable → refusal.
/// 5. If no identity resolved and operation requires it → `MissingNodeIdentity`.
pub fn resolve_identity(
    cli_node_id: Option<u32>,
    single_node_dev: bool,
    profile: &str,
    _operation_name: &str,
) -> Result<CliResolvedIdentity, CliIdentityResolveError> {
    // 1. Conflict check: both flags set.
    if cli_node_id.is_some() && single_node_dev {
        return Err(CliIdentityResolveError::ConflictingFlags);
    }

    // 2. Explicit --node-id wins.
    if let Some(id) = cli_node_id {
        let id_i32 = id as i32;
        if !validate_node_id_range(id_i32) {
            return Err(CliIdentityResolveError::NodeIdOutOfRange { value: id_i32 });
        }
        return Ok(CliResolvedIdentity::Selected(id_i32));
    }

    // 3. --single-node-dev.
    if single_node_dev {
        let is_prod = profile == "production" || is_production_env();
        if is_prod {
            return Err(CliIdentityResolveError::SingleNodeDevRefusedInProduction);
        }
        return Ok(CliResolvedIdentity::SingleNodeDev);
    }

    // 4. Fall back to HEER_NODE_ID env var.
    if let Ok(env_val) = std::env::var("HEER_NODE_ID") {
        match env_val.parse::<i32>() {
            Ok(id) if validate_node_id_range(id) => {
                return Ok(CliResolvedIdentity::Selected(id));
            }
            Ok(id) => {
                return Err(CliIdentityResolveError::NodeIdOutOfRange { value: id });
            }
            Err(_) => {
                // Env var exists but is not a valid integer.
                return Err(CliIdentityResolveError::InvalidEnvFormat { value: env_val });
            }
        }
    }

    // 5. No identity resolved.
    Err(CliIdentityResolveError::MissingNodeIdentity)
}

/// Convert a [`CliResolvedIdentity`] into the library's
/// [`djogi::migrate::RunnerIdentity`].
impl CliResolvedIdentity {
    pub fn into_runner_identity(self) -> djogi::migrate::RunnerIdentity {
        match self {
            CliResolvedIdentity::Selected(id) => djogi::migrate::RunnerIdentity::Selected { id },
            CliResolvedIdentity::SingleNodeDev => djogi::migrate::RunnerIdentity::SingleNodeDev,
        }
    }
}

/// Print a CLI identity resolution error to stderr and return the
/// appropriate exit code. All identity resolution errors are exit code 2
/// (refusal — operator must intervene before retrying).
pub fn print_identity_error(operation: &str, err: &CliIdentityResolveError) -> ExitCode {
    eprintln!("djogi migrations {operation}: refused — {err}");
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_explicit_node_id_selected() {
        // Explicit --node-id wins.
        let result = resolve_identity(Some(7), false, "development", "apply");
        assert_eq!(result, Ok(CliResolvedIdentity::Selected(7)));
    }

    #[test]
    fn resolve_explicit_node_id_zero_boundary() {
        // Node ID 0 is within valid range.
        let result = resolve_identity(Some(0), false, "development", "apply");
        assert_eq!(result, Ok(CliResolvedIdentity::Selected(0)));
    }

    #[test]
    fn resolve_explicit_node_id_upper_boundary() {
        // Node ID 511 is within valid range.
        let result = resolve_identity(Some(511), false, "development", "apply");
        assert_eq!(result, Ok(CliResolvedIdentity::Selected(511)));
    }

    #[test]
    fn resolve_node_id_out_of_range_below() {
        // Node ID -1 (from u32 wrapping, so 512 from CLI which is above range).
        let result = resolve_identity(Some(512), false, "development", "apply");
        assert_eq!(
            result,
            Err(CliIdentityResolveError::NodeIdOutOfRange { value: 512 })
        );
    }

    #[test]
    fn resolve_node_id_out_of_range_max() {
        // u32::MAX is way out of range.
        let result = resolve_identity(Some(u32::MAX), false, "development", "apply");
        assert_eq!(
            result,
            Err(CliIdentityResolveError::NodeIdOutOfRange {
                value: u32::MAX as i32
            })
        );
    }

    #[test]
    fn resolve_single_node_dev_allowed_in_dev() {
        let result = resolve_identity(None, true, "development", "apply");
        assert_eq!(result, Ok(CliResolvedIdentity::SingleNodeDev));
    }

    #[test]
    fn resolve_single_node_dev_refused_in_production_profile() {
        let result = resolve_identity(None, true, "production", "apply");
        assert_eq!(
            result,
            Err(CliIdentityResolveError::SingleNodeDevRefusedInProduction)
        );
    }

    #[test]
    fn resolve_conflicting_flags() {
        let result = resolve_identity(Some(1), true, "development", "apply");
        assert_eq!(result, Err(CliIdentityResolveError::ConflictingFlags));
    }

    #[test]
    fn resolve_missing_identity() {
        // Make sure HEER_NODE_ID is not set.
        unsafe { std::env::remove_var("HEER_NODE_ID"); }
        let result = resolve_identity(None, false, "development", "apply");
        assert_eq!(result, Err(CliIdentityResolveError::MissingNodeIdentity));
    }

    #[test]
    fn resolve_env_var_fallback() {
        unsafe { std::env::set_var("HEER_NODE_ID", "42"); }
        let result = resolve_identity(None, false, "development", "apply");
        assert_eq!(result, Ok(CliResolvedIdentity::Selected(42)));
        unsafe { std::env::remove_var("HEER_NODE_ID"); }
    }

    #[test]
    fn resolve_explicit_wins_over_env() {
        unsafe { std::env::set_var("HEER_NODE_ID", "99"); }
        let result = resolve_identity(Some(7), false, "development", "apply");
        assert_eq!(result, Ok(CliResolvedIdentity::Selected(7)));
        unsafe { std::env::remove_var("HEER_NODE_ID"); }
    }

    #[test]
    fn resolve_env_var_out_of_range() {
        unsafe { std::env::set_var("HEER_NODE_ID", "9999"); }
        let result = resolve_identity(None, false, "development", "apply");
        assert_eq!(
            result,
            Err(CliIdentityResolveError::NodeIdOutOfRange { value: 9999 })
        );
        unsafe { std::env::remove_var("HEER_NODE_ID"); }
    }

    #[test]
    fn resolve_env_var_unparseable() {
        unsafe { std::env::set_var("HEER_NODE_ID", "not-a-number"); }
        let result = resolve_identity(None, false, "development", "apply");
        // Unparseable env var returns InvalidEnvFormat, not MissingNodeIdentity.
        assert_eq!(
            result,
            Err(CliIdentityResolveError::InvalidEnvFormat {
                value: "not-a-number".to_string()
            })
        );
        unsafe { std::env::remove_var("HEER_NODE_ID"); }
    }

    #[test]
    fn validate_node_id_range_boundaries() {
        assert!(validate_node_id_range(0));
        assert!(validate_node_id_range(1));
        assert!(validate_node_id_range(256));
        assert!(validate_node_id_range(511));
        assert!(!validate_node_id_range(-1));
        assert!(!validate_node_id_range(512));
    }

    #[test]
    fn resolve_single_node_dev_refused_in_djogi_env_production() {
        unsafe { std::env::set_var("DJOGI_ENV", "production"); }
        let result = resolve_identity(None, true, "development", "apply");
        assert_eq!(
            result,
            Err(CliIdentityResolveError::SingleNodeDevRefusedInProduction)
        );
        unsafe { std::env::remove_var("DJOGI_ENV"); }
    }

    #[test]
    fn resolve_single_node_dev_allowed_in_djogi_env_development() {
        unsafe { std::env::set_var("DJOGI_ENV", "development"); }
        let result = resolve_identity(None, true, "development", "apply");
        assert_eq!(result, Ok(CliResolvedIdentity::SingleNodeDev));
        unsafe { std::env::remove_var("DJOGI_ENV"); }
    }
}
