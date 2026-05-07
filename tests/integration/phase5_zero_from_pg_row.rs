use djogi::FromPgRow;
use djogi::prelude::*;
use serde::{Deserialize, Serialize};

#[model(table = "t3_probes")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct T3Probe {
    pub label: String,
    pub count: i32,
    pub flag: bool,
}

#[test]
fn columns_in_struct_field_order() {
    assert_eq!(
        <T3Probe as FromPgRow>::COLUMNS,
        &["id", "created_at", "updated_at", "label", "count", "flag"],
    );
}

#[test]
fn column_list_is_comma_joined() {
    assert_eq!(
        <T3Probe as FromPgRow>::COLUMN_LIST,
        "id, created_at, updated_at, label, count, flag",
    );
}

#[djogi::djogi_test(sync_models = [T3Probe])]
async fn from_pg_row_round_trips_ordinal_decode(mut ctx: djogi::DjogiContext) {
    let created = T3Probe::create(
        &mut ctx,
        T3Probe {
            label: "hello".into(),
            count: 42,
            flag: true,
            ..Default::default()
        },
    )
    .await
    .expect("create should succeed");

    assert_eq!(created.label, "hello");
    assert_eq!(created.count, 42);
    assert!(created.flag);

    let fetched = T3Probe::get(&mut ctx, created.id)
        .await
        .expect("get should succeed");
    assert_eq!(fetched, created);
}
