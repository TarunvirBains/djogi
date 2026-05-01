//! Smoke test: verify we can connect to Postgres and run a basic query.

// figment::Error is a large external type we cannot shrink; these closures
// return Result<(), figment::Error> as required by figment::Jail::expect_with.
#![allow(clippy::result_large_err)]

use djogi::config::DjogiConfig;

#[djogi::djogi_test]
async fn connects_to_postgres(mut ctx: djogi::DjogiContext) {
    let row = ctx
        .__query_one_for_macros("SELECT 1::integer AS val", &[])
        .await
        .expect("failed to run SELECT 1");
    let val: i32 = row.try_get("val").expect("val column should be i32");
    assert_eq!(val, 1);
}

#[djogi::djogi_test]
async fn postgres_version_is_18(mut ctx: djogi::DjogiContext) {
    let row = ctx
        .__query_one_for_macros("SELECT version() AS v", &[])
        .await
        .expect("failed to get version");
    let version: String = row.try_get("v").expect("v column should be text");
    assert!(
        version.contains("PostgreSQL 18"),
        "Expected PostgreSQL 18, got: {}",
        version
    );
}

#[test]
fn default_config_has_sensible_defaults() {
    figment::Jail::expect_with(|jail| {
        jail.set_env("DATABASE_URL", "postgres://localhost/test");
        let config = DjogiConfig::load()?;
        assert!(config.database.max_connections.is_none());
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

#[djogi::djogi_test]
async fn heeranjid_generates_id(mut ctx: djogi::DjogiContext) {
    // HeeRanjID schema is already installed and node seeded by #[djogi_test] bootstrap.
    let row = ctx
        .__query_one_for_macros("SELECT generate_id() AS id", &[])
        .await
        .expect("failed to call generate_id()");
    let id: i64 = row.try_get("id").expect("id column should be i64");
    assert!(id > 0, "Expected positive HeerId, got: {}", id);
}
