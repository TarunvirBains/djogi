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
}
