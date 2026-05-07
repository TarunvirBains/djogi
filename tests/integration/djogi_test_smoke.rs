// Smoke test for the `#[djogi_test]` proc-macro lifecycle.
//
// # What this proves
//
// The smoke test exercises the full lifecycle end-to-end:
// 1. A per-test Postgres database is created (`djogi_test_<uuid>`).
// 2. HeeRanjID schema is installed and the default node seeded.
// 3. A `DjogiContext` is constructed and passed to the test body.
// 4. The context is usable through the typed Model and QuerySet APIs.
// 5. The database is dropped after the test body returns.

use djogi::prelude::*;

#[model(table = "djogi_test_probe_widgets", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct ProbeWidget {
    pub name: String,
}

#[djogi::djogi_test(sync_models = [ProbeWidget])]
async fn djogi_test_context_is_usable(mut ctx: djogi::DjogiContext) {
    let created = ProbeWidget::create(
        &mut ctx,
        ProbeWidget {
            name: "macro-smoke".into(),
            ..Default::default()
        },
    )
    .await
    .expect("ProbeWidget::create should succeed");

    let reloaded = ProbeWidget::get(&mut ctx, created.id)
        .await
        .expect("ProbeWidget::get should reload the row");
    assert_eq!(reloaded.name, "macro-smoke");
}

#[djogi::djogi_test(sync_models = [ProbeWidget])]
async fn djogi_test_heeranjid_default_is_installed(mut ctx: djogi::DjogiContext) {
    let created = ProbeWidget::create(
        &mut ctx,
        ProbeWidget {
            name: "id-smoke".into(),
            ..Default::default()
        },
    )
    .await
    .expect("ProbeWidget::create should use the HeerId default");

    assert!(
        created.id.as_i64() > 0,
        "DB-generated HeerId should be positive"
    );
}
