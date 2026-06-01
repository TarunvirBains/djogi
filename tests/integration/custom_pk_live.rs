// Typed live-DB coverage for DB-generated primary keys.
//
// The previous version of this file exercised adopter-defined primary-key
// declarations and handwritten sequence/table setup. The raw-surface rewrite
// keeps this source on the ordinary Djogi surface: model declaration,
// `sync_models`, CRUD, descriptor inspection, and `PrimaryKeyDbGen`.

use djogi::prelude::*;
use djogi::types::HeerId;

#[model(table = "live_custom_pk_rows", pk = HeerId)]
#[derive(Debug, Clone)]
pub struct LiveRow {
    pub label: String,
}

#[djogi::djogi_test(sync_models = [LiveRow])]
async fn db_generated_pk_create_and_fetch_round_trip(mut ctx: DjogiContext) {
    let created = LiveRow::create(
        &mut ctx,
        LiveRow {
            label: "round-trip".into(),
            ..Default::default()
        },
    )
    .await
    .expect("create() must succeed with a DB-generated PK");

    assert_ne!(
        created.id,
        <HeerId as PrimaryKey>::sentinel(),
        "DB-sourced PK must not be the sentinel"
    );
    assert_eq!(created.label, "round-trip");

    let fetched = LiveRow::get(&mut ctx, created.id)
        .await
        .expect("get() by PK must resolve the row we just inserted");
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.label, "round-trip");
}

#[djogi::djogi_test(sync_models = [LiveRow])]
async fn db_generated_pk_descriptor_carries_builtin_kind(mut ctx: DjogiContext) {
    let _ = &mut ctx;

    let descriptor = <LiveRow as ::djogi::model::Model>::descriptor();
    assert!(matches!(descriptor.pk_type, ::djogi::PkType::HeerId));
}
