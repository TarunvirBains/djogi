#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): exercises raw_ddl itself
#[djogi::djogi_test]
async fn raw_ddl_runs_simple_query_protocol(mut ctx: djogi::DjogiContext) {
    ctx.raw_ddl("CREATE TEMP TABLE raw_ddl_pin_values (value integer NOT NULL)")
        .await
        .expect("raw_ddl should run DDL");

    ctx.raw_execute("INSERT INTO raw_ddl_pin_values (value) VALUES (46)", &[])
        .await
        .expect("created table should be usable");
    let rows = ctx
        .raw_rows("SELECT value FROM raw_ddl_pin_values", &[])
        .await
        .expect("inserted row should be visible");
    let row = rows.into_iter().next().expect("exactly one row");
    let value: i32 = row.try_get("value").expect("value column decodes");

    assert_eq!(value, 46);
}
