// Smoke test: verify the typed Djogi test harness can create schema and
// round-trip a model against Postgres.

// figment::Error is a large external type we cannot shrink; these closures
// return Result<(), figment::Error> as required by figment::Jail::expect_with.
use djogi::config::DjogiConfig;
use djogi::prelude::*;
use djogi::testing::{setup_test_db, teardown_test_db};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::{Mutex, OnceLock};

#[model(table = "smoke_widgets", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct SmokeWidget {
    pub name: String,
}

fn smoke_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
#[allow(clippy::result_large_err)]
fn default_config_has_sensible_defaults() {
    let _guard = smoke_env_lock()
        .lock()
        .expect("smoke env lock must not be poisoned");
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
#[allow(clippy::result_large_err)]
fn database_url_env_overrides_toml() {
    let _guard = smoke_env_lock()
        .lock()
        .expect("smoke env lock must not be poisoned");
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

#[test]
fn typed_model_round_trip_uses_postgres() {
    let _guard = smoke_env_lock()
        .lock()
        .expect("smoke env lock must not be poisoned");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current_thread Tokio runtime");

    runtime.block_on(async {
        let (cleanup, mut ctx) = setup_test_db()
            .await
            .expect("setup_test_db must succeed against DATABASE_URL");

        let outcome = AssertUnwindSafe(async {
            djogi::testing::sync_models(&mut ctx, &[<SmokeWidget as Model>::descriptor()])
                .await
                .expect("sync_models must create the smoke fixture table");

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
        })
        .catch_unwind()
        .await;

        teardown_test_db(cleanup).await;
        if let Err(payload) = outcome {
            std::panic::resume_unwind(payload);
        }
    });
}
