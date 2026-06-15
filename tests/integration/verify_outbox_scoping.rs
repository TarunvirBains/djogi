use djogi::migrate::{BucketKey, VerifyReport, project_from_inventory, verify_bucket};
use djogi::prelude::*;

djogi::apps! {
    #[app(database = "main")]
    pub struct Journal;
}

#[model(table = "notes", app = Journal, events)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct Note {
    pub title: String,
}

fn bucket(app: &str) -> BucketKey {
    BucketKey {
        database: "main".to_string(),
        app: app.to_string(),
    }
}

fn has_outbox_code(report: &VerifyReport, code: &str, table: &str) -> bool {
    report
        .diagnostics
        .iter()
        .any(|d| d.code == code && d.location.as_deref() == Some(table))
}

#[djogi::djogi_test(sync_models = [Note])]
async fn verify_bucket_named_app_outbox_no_contradictory_diagnostics(mut ctx: djogi::DjogiContext) {
    let projected =
        project_from_inventory().expect("project_from_inventory must succeed for Note inventory");
    let named_bucket = bucket("journal");
    let global_bucket = bucket("");
    let named_snapshot = projected
        .get(&named_bucket)
        .expect("journal bucket must exist in projected snapshot");
    let global_snapshot = projected
        .get(&global_bucket)
        .expect("global bucket must exist in projected snapshot");
    let policy = djogi::config::PolicyConfig::default();

    let named_report = verify_bucket(&mut ctx, &named_bucket, named_snapshot, &policy, true, true)
        .await
        .expect("named bucket verify must succeed");

    let global_report = verify_bucket(
        &mut ctx,
        &global_bucket,
        global_snapshot,
        &policy,
        false,
        true,
    )
    .await
    .expect("global bucket verify must succeed");

    assert!(
        !has_outbox_code(&named_report, "D601", "notes_outbox"),
        "named bucket must not report notes_outbox missing: {:?}",
        named_report.diagnostics
    );
    assert!(
        !has_outbox_code(&named_report, "D602", "notes_outbox"),
        "named bucket must not report notes_outbox extra: {:?}",
        named_report.diagnostics
    );
    assert!(
        !has_outbox_code(&global_report, "D601", "notes_outbox"),
        "global bucket must not report notes_outbox missing: {:?}",
        global_report.diagnostics
    );
    assert!(
        !has_outbox_code(&global_report, "D602", "notes_outbox"),
        "global bucket must not report notes_outbox extra: {:?}",
        global_report.diagnostics
    );
}
