//! Configuration via `Djogi.toml` + environment variables.
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
    /// CRUD/audit database URL. When `None`, audit surfaces derive
    /// `crud_log` from [`url`](Self::url) unless an environment variable
    /// override is present.
    #[serde(default)]
    pub crud_log_url: Option<String>,
    /// Event/observability database URL. Reserved for the event-log
    /// pool surface; stored here so `Djogi.toml` matches the documented
    /// three-database architecture even before every consumer is wired.
    #[serde(default)]
    pub event_log_url: Option<String>,
    /// Connection-pool size override. `None` (or absent in TOML) means
    /// the env > Djogi.toml > builder-default chain falls through to
    /// the builder default; an explicit non-zero value here overrides
    /// the default. Zero is treated identically to `None` so a user
    /// typo cannot silently zero the pool.
    #[serde(default)]
    pub max_connections: Option<u32>,
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
/// Per .5 — when a transactional `CREATE INDEX` is
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

    /// Threshold (in seconds) above which an open transaction
    /// triggers the pre-flight refusal in the PK-flip orchestration.
    /// The runner enumerates `pg_stat_activity` rows whose
    /// `xact_start` is older than `now() - INTERVAL <threshold>`
    /// before opening the cutover transaction; any rows found refuse
    /// the cutover with `RunnerError::PkFlipHazardLongRunningTx`.
    /// Default `60` seconds. Set to `0` to disable the check.
    #[serde(default = "default_pk_flip_long_tx_threshold_secs")]
    pub pk_flip_long_tx_threshold_secs: u32,

    /// Join-table cutover layout for PK-flip orchestration.
    /// `'A'` (default — uppercase ASCII letter A) emits a single
    /// mega-transaction across both parents and the join table per
    /// playbook §7. `'B'` emits sequential per-parent migrations
    /// each of which is a self-contained PkTypeFlipGroup. The
    /// compose pipeline reads this knob to decide between the two
    /// layouts. Operators flip to `'B'` when their reviewers prefer
    /// narrower windows over the atomic mega-tx invariant.
    #[serde(default = "default_pk_flip_join_table_option")]
    pub pk_flip_join_table_option: char,
}

fn default_pk_flip_long_tx_threshold_secs() -> u32 {
    60
}

fn default_pk_flip_join_table_option() -> char {
    'A'
}

impl Default for MigrateConfig {
    fn default() -> Self {
        Self {
            concurrent_warn_relpages: 128,
            strict_concurrent_warnings: false,
            pk_flip_long_tx_threshold_secs: default_pk_flip_long_tx_threshold_secs(),
            pk_flip_join_table_option: default_pk_flip_join_table_option(),
        }
    }
}

/// Migration policy knobs — controls how the runner reacts to
/// out-of-order applies and how `verify` reports historical
/// out-of-order rows.
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
                crud_log_url: None,
                event_log_url: None,
                max_connections: None,
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
        if let Ok(url) = std::env::var("CRUD_LOG_URL")
            && !url.is_empty()
        {
            config.database.crud_log_url = Some(url);
        }
        if let Ok(url) = std::env::var("EVENT_LOG_URL")
            && !url.is_empty()
        {
            config.database.event_log_url = Some(url);
        }

        Ok(config)
    }

    /// Load configuration from `<workspace>/Djogi.toml` instead of the
    /// cwd-relative `Djogi.toml`.
    /// This is the path-aware loader used by CLI subcommands that
    /// accept `--workspace <path>`. The default
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
        if let Ok(url) = std::env::var("CRUD_LOG_URL")
            && !url.is_empty()
        {
            config.database.crud_log_url = Some(url);
        }
        if let Ok(url) = std::env::var("EVENT_LOG_URL")
            && !url.is_empty()
        {
            config.database.event_log_url = Some(url);
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
    fn database_max_connections_default_is_none() {
        let cfg = DjogiConfig::default();
        assert!(cfg.database.max_connections.is_none());
    }

    /// Loading a TOML that omits `[database].max_connections` keeps
    /// `None` rather than silently substituting a non-zero default
    /// the `from_database_config` path must be able to fall through to
    /// the builder default.
    #[test]
    #[allow(clippy::result_large_err)] // Jail returns figment::Error
    fn loaded_config_without_max_connections_is_none() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "Djogi.toml",
                r#"
                [database]
                url = "postgres://localhost/test"
                dev_mode = false

                [server]
                host = "0.0.0.0"
                port = 8000
                "#,
            )?;
            // No DATABASE_URL or DJOGI_DATABASE_MAX_CONNECTIONS in jail.
            let cfg = DjogiConfig::load().expect("load should succeed");
            assert!(
                cfg.database.max_connections.is_none(),
                "TOML without max_connections must remain None"
            );
            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)] // Jail returns figment::Error
    fn loaded_config_reads_three_database_urls_from_toml() {
        let _guard = crate::migrate::audit::AUDIT_URL_ENV_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        figment::Jail::expect_with(|jail| {
            // Empty process env values are treated as unset, so this
            // test pins the TOML surface without inheriting a developer
            // shell's CRUD_LOG_URL / EVENT_LOG_URL.
            jail.set_env("CRUD_LOG_URL", "");
            jail.set_env("EVENT_LOG_URL", "");
            jail.create_file(
                "Djogi.toml",
                r#"
                [database]
                url = "postgres://localhost/app"
                crud_log_url = "postgres://localhost/crud_log"
                event_log_url = "postgres://localhost/event_log"
                dev_mode = false

                [server]
                host = "0.0.0.0"
                port = 8000
                "#,
            )?;

            let cfg = DjogiConfig::load().expect("load should succeed");
            assert_eq!(
                cfg.database.crud_log_url.as_deref(),
                Some("postgres://localhost/crud_log")
            );
            assert_eq!(
                cfg.database.event_log_url.as_deref(),
                Some("postgres://localhost/event_log")
            );
            Ok(())
        });
    }

    #[test]
    #[allow(clippy::result_large_err)] // Jail returns figment::Error
    fn unprefixed_log_url_env_vars_override_toml() {
        let _guard = crate::migrate::audit::AUDIT_URL_ENV_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        figment::Jail::expect_with(|jail| {
            jail.set_env("CRUD_LOG_URL", "postgres://localhost/env_crud_log");
            jail.set_env("EVENT_LOG_URL", "postgres://localhost/env_event_log");
            jail.create_file(
                "Djogi.toml",
                r#"
                [database]
                url = "postgres://localhost/app"
                crud_log_url = "postgres://localhost/toml_crud_log"
                event_log_url = "postgres://localhost/toml_event_log"
                dev_mode = false

                [server]
                host = "0.0.0.0"
                port = 8000
                "#,
            )?;

            let cfg = DjogiConfig::load().expect("load should succeed");
            assert_eq!(
                cfg.database.crud_log_url.as_deref(),
                Some("postgres://localhost/env_crud_log")
            );
            assert_eq!(
                cfg.database.event_log_url.as_deref(),
                Some("postgres://localhost/env_event_log")
            );
            Ok(())
        });
    }

    #[test]
    fn policy_config_default_is_lenient() {
        let cfg = DjogiConfig::default();
        assert!(!cfg.policy.strict_out_of_order);
    }
}
