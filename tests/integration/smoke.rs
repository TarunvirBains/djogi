//! Smoke test: verify we can connect to Postgres and run a basic query.

use djogi::config::DjogiConfig;
use heeranjid_sqlx::{generate_heerid, install_schema, seed_default_node};
use sqlx::PgPool;

#[sqlx::test]
async fn connects_to_postgres(pool: PgPool) {
    let row: (i32,) = sqlx::query_as("SELECT 1")
        .fetch_one(&pool)
        .await
        .expect("failed to run SELECT 1");
    assert_eq!(row.0, 1);
}

#[sqlx::test]
async fn postgres_version_is_16(pool: PgPool) {
    let row: (String,) = sqlx::query_as("SELECT version()")
        .fetch_one(&pool)
        .await
        .expect("failed to get version");
    assert!(
        row.0.contains("PostgreSQL 16"),
        "Expected PostgreSQL 16, got: {}",
        row.0
    );
}

#[test]
fn default_config_has_sensible_defaults() {
    figment::Jail::expect_with(|jail| {
        jail.set_env("DATABASE_URL", "postgres://localhost/test");
        let config = DjogiConfig::load()?;
        assert_eq!(config.database.max_connections, 10);
        assert!(!config.database.dev_mode);
        assert_eq!(config.server.port, 8000);
        Ok(())
    });
}

#[test]
fn database_url_env_overrides_toml() {
    figment::Jail::expect_with(|jail| {
        jail.create_file(
            "Djogi.toml",
            r#"
            [database]
            url = "postgres://localhost/from_toml"
            "#,
        )?;
        jail.set_env("DATABASE_URL", "postgres://localhost/from_env");
        let config = DjogiConfig::load()?;
        assert_eq!(config.database.url, "postgres://localhost/from_env");
        Ok(())
    });
}

#[sqlx::test]
async fn heeranjid_generates_id(pool: PgPool) {
    install_schema(&pool)
        .await
        .expect("failed to install heeranjid schema");
    seed_default_node(&pool)
        .await
        .expect("failed to seed default node");

    let id = generate_heerid(&pool, 1)
        .await
        .expect("failed to generate heerid");

    assert!(
        id.as_i64() > 0,
        "Expected positive HeerId, got: {}",
        id.as_i64()
    );
}
