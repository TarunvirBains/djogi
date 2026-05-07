#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): exercises raw_scalar itself
#[djogi::djogi_test]
async fn raw_scalar_decodes_one_scalar(mut ctx: djogi::DjogiContext) {
    let value = ctx
        .raw_scalar::<i64>("SELECT $1::bigint", &[&44_i64])
        .await
        .expect("raw_scalar should decode one scalar");

    assert_eq!(value, 44);
}
