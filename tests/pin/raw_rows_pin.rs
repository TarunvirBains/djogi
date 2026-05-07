#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): exercises raw_rows itself
#[djogi::djogi_test]
async fn raw_rows_returns_driver_rows(mut ctx: djogi::DjogiContext) {
    let rows = ctx
        .raw_rows("SELECT $1::integer AS value", &[&42_i32])
        .await
        .expect("raw_rows should return raw driver rows");

    let value: i32 = rows[0].try_get("value").expect("value column decodes");
    assert_eq!(value, 42);
}
