#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): exercises raw_execute itself
#[djogi::djogi_test]
async fn raw_execute_reports_affected_rows(mut ctx: djogi::DjogiContext) {
    ctx.raw_ddl("CREATE TEMP TABLE raw_execute_pin_values (value integer NOT NULL)")
        .await
        .expect("temp table should be created");

    let affected = ctx
        .raw_execute(
            "INSERT INTO raw_execute_pin_values (value) VALUES ($1)",
            &[&45_i32],
        )
        .await
        .expect("raw_execute should run parameterized DML");

    let rows = ctx
        .raw_rows("SELECT value FROM raw_execute_pin_values", &[])
        .await
        .expect("inserted row should be visible");
    let row = rows.into_iter().next().expect("exactly one row");
    let value: i32 = row.try_get("value").expect("value column decodes");

    assert_eq!(affected, 1);
    assert_eq!(value, 45);
}
