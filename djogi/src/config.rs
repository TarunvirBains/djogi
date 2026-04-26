//! Configuration via `Djogi.toml` + environment variables.
//!
//! `DATABASE_URL` env var always overrides `[database].url`.
//! Secrets live in env vars, never in `Djogi.toml`.

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct DjogiConfig {
    pub database: DatabaseConfig,
    pub server: ServerConfig,
    /// Migration-engine settings. See [`MigrateConfig`].
    #[serde(default)]
    pub migrate: MigrateConfig,
    /// Deployment profile. Drives the migration engine's
    /// out-of-order policy default (production/CI rejects out-of-order
    /// applies; development warns and proceeds) and gates destructive
    /// `attune --squash` operations.
    ///
    /// Recognised values today: `"development"` (default), `"production"`,
    /// `"staging"`, `"test"`. Anything that is not the literal
    /// `"production"` string is treated as non-production.
    #[serde(default = "default_profile")]
    pub profile: String,
    /// Migration policy knobs — orthogonal to [`MigrateConfig`]
    /// (which controls runner behaviour like the relpages probe).
    /// Policy fields gate which apply paths the runner accepts and how
    /// loud `verify` is about historical drift.
    #[serde(default)]
    pub policy: PolicyConfig,
}

/// Default value for [`DjogiConfig::profile`] when the field is absent
/// from `Djogi.toml`. Development is the safe default for new
/// projects — production environments must opt in explicitly.
fn default_profile() -> String {
    "development".to_string()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct DatabaseConfig {
    pub url: String,
    pub max_connections: u32,
    pub dev_mode: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

/// Migration-engine settings. Controls the runner's relpages-probe
/// behaviour for `CREATE INDEX` statements that lack
/// `requires_out_of_transaction = true`.
///
/// Per Phase 7-Zero v3 §6.5 — when a transactional `CREATE INDEX` is
/// about to run against a table whose `pg_class.relpages` exceeds
/// `concurrent_warn_relpages`, the runner emits a
/// `tracing::warn!` advising the operator to opt the index into
/// `CREATE INDEX CONCURRENTLY` (which would set
/// `requires_out_of_transaction = true` upstream). When
/// `strict_concurrent_warnings` is `true` the runner aborts the apply
/// with a `RunnerError::RelpagesThresholdExceeded` instead — failing
/// loudly is the production stance for environments that cannot
/// tolerate a long ACCESS-EXCLUSIVE lock.
#[derive(Debug, Deserialize, Serialize)]
pub struct MigrateConfig {
    /// `pg_class.relpages` threshold above which a transactional
    /// `CREATE INDEX` triggers the advisory probe. Default `128`
    /// (≈1 MB at the standard 8 kB page size). Set to `u32::MAX` to
    /// disable the probe entirely.
    pub concurrent_warn_relpages: u32,

    /// When `true`, the relpages probe upgrades from a `tracing::warn!`
    /// to a hard error (`RunnerError::RelpagesThresholdExceeded`).
    /// Default `false` so dev iteration is unblocked; production
    /// configs typically flip this on.
    pub strict_concurrent_warnings: bool,
}

impl Default for MigrateConfig {
    fn default() -> Self {
        Self {
            concurrent_warn_relpages: 128,
            strict_concurrent_warnings: false,
        }
    }
}

/// Migration policy knobs — controls how the runner reacts to
/// out-of-order applies and how `verify` reports historical
/// out-of-order rows.
///
/// These fields are intentionally separate from [`MigrateConfig`].
/// `MigrateConfig` controls runner mechanics (relpages probe, strict
/// warnings) — `PolicyConfig` controls policy decisions (allow vs
/// reject). The split lets an operator dial mechanics independently
/// from policy.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct PolicyConfig {
    /// When `true`, [`crate::migrate::verify`] surfaces out-of-order
    /// rows as `D622` Error diagnostics (verify exits non-zero).
    /// When `false` (the default), the same rows surface as `D622`
    /// Warning — verify still reports the drift, but does not fail
    /// the run.
    ///
    /// Pair with the runner-side [`crate::migrate::OutOfOrderPolicy`]:
    /// the runner gates whether out-of-order applies are PERMITTED;
    /// `strict_out_of_order` gates whether already-applied out-of-order
    /// rows count as a verify-time ERROR or just a warning. Production
    /// environments that have already cleaned up historical drift
    /// flip this on to make new drift hard to ignore.
    #[serde(default)]
    pub strict_out_of_order: bool,
}

impl Default for DjogiConfig {
    fn default() -> Self {
        Self {
            database: DatabaseConfig {
                url: String::new(),
                max_connections: 10,
                dev_mode: false,
            },
            server: ServerConfig {
                host: "0.0.0.0".into(),
                port: 8000,
            },
            migrate: MigrateConfig::default(),
            profile: default_profile(),
            policy: PolicyConfig::default(),
        }
    }
}

impl DjogiConfig {
    /// Returns `true` when this configuration represents a production
    /// deployment. Used by the migration engine to set the default
    /// [`crate::migrate::OutOfOrderPolicy`] to `Reject` and to gate
    /// `attune --squash` against accidental destructive history
    /// rewrites.
    ///
    /// **Definition.** `profile` literally equal to the lowercase
    /// string `"production"`. Anything else (including
    /// `"Production"`, `"PROD"`, `"prod"`) is treated as
    /// non-production. The strictness is intentional — a typo in the
    /// profile field should fall back to the safe-for-dev default,
    /// not silently flip the runner into reject-mode.
    pub fn is_production(&self) -> bool {
        self.profile == "production"
    }
}

impl DjogiConfig {
    /// Load configuration from `Djogi.toml` (if present) merged with
    /// environment variables. `DATABASE_URL` overrides `database.url`.
    #[allow(clippy::result_large_err)]
    pub fn load() -> Result<Self, figment::Error> {
        let mut config: DjogiConfig = Figment::new()
            .merge(Serialized::defaults(DjogiConfig::default()))
            .merge(Toml::file("Djogi.toml"))
            .merge(Env::prefixed("DJOGI_").split("_"))
            .extract()?;

        // DATABASE_URL env var always wins (not prefixed with DJOGI_)
        if let Ok(url) = std::env::var("DATABASE_URL") {
            config.database.url = url;
        }

        Ok(config)
    }

    /// Load configuration from `<workspace>/Djogi.toml` instead of the
    /// cwd-relative `Djogi.toml`.
    ///
    /// This is the path-aware loader used by CLI subcommands that
    /// accept `--workspace <path>` (per Codex A-1). The default
    /// [`load`](Self::load) reads `Djogi.toml` from the current
    /// working directory; callers that want to operate against a
    /// different workspace pass the resolved path here. Environment-
    /// variable overrides (`DATABASE_URL`, `DJOGI_*`) still win, so
    /// secrets stay out of the workspace config.
    #[allow(clippy::result_large_err)]
    pub fn load_from_workspace(workspace: &std::path::Path) -> Result<Self, figment::Error> {
        let toml_path = workspace.join("Djogi.toml");
        let mut config: DjogiConfig = Figment::new()
            .merge(Serialized::defaults(DjogiConfig::default()))
            .merge(Toml::file(toml_path))
            .merge(Env::prefixed("DJOGI_").split("_"))
            .extract()?;

        // DATABASE_URL env var always wins (not prefixed with DJOGI_)
        if let Ok(url) = std::env::var("DATABASE_URL") {
            config.database.url = url;
        }

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_is_development() {
        let cfg = DjogiConfig::default();
        assert_eq!(cfg.profile, "development");
        assert!(!cfg.is_production());
    }

    #[test]
    fn profile_eq_production_only_for_exact_lowercase_match() {
        // Helper: build a default config with the supplied profile string.
        let with_profile = |s: &str| DjogiConfig {
            profile: s.to_string(),
            ..DjogiConfig::default()
        };
        assert!(with_profile("production").is_production());

        // Strict — typos must fall back to non-production.
        assert!(!with_profile("Production").is_production());
        assert!(!with_profile("PROD").is_production());
        assert!(!with_profile("prod").is_production());
        assert!(!with_profile("staging").is_production());
        assert!(!with_profile("test").is_production());
        assert!(!with_profile("").is_production());
    }

    #[test]
    fn policy_config_default_is_lenient() {
        let cfg = DjogiConfig::default();
        assert!(!cfg.policy.strict_out_of_order);
    }
}
