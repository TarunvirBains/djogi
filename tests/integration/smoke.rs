// Smoke test: verify the typed Djogi test harness can create schema and
// round-trip a model against Postgres.

// figment::Error is a large external type we cannot shrink; these closures
// return Result<(), figment::Error> as required by figment::Jail::expect_with.
use djogi::config::DjogiConfig;
use djogi::prelude::*;

#[model(table = "smoke_widgets", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct SmokeWidget {
    pub name: String,
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

#[djogi::djogi_test(sync_models = [SmokeWidget])]
async fn typed_model_round_trip_uses_postgres(mut ctx: djogi::DjogiContext) {
    let created = SmokeWidget::create(
        &mut ctx,
        SmokeWidget {
            name: "typed-smoke".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create SmokeWidget");

    assert!(
        created.id.as_i64() > 0,
        "DB-generated HeerId must be positive"
    );

    let reloaded = SmokeWidget::get(&mut ctx, created.id)
        .await
        .expect("get SmokeWidget");
    assert_eq!(reloaded.name, "typed-smoke");
}
