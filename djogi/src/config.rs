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
        }
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
