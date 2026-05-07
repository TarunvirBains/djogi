struct RawQueryProbe {
    value: i32,
}

impl djogi::FromPgRow for RawQueryProbe {
    const COLUMNS: &'static [&'static str] = &["value"];
    const COLUMN_LIST: &'static str = "value";

    fn from_pg_row(row: &tokio_postgres::Row) -> Result<Self, djogi::DjogiError> {
        Ok(Self {
            value: row
                .try_get("value")
                .map_err(|e| djogi::DjogiError::Decode(format!("column `value`: {e}")))?,
        })
    }
}

#[djogi::deliberately_bypass_convention_with_raw_sql]
// JUSTIFICATION (PIN): exercises raw_query itself
#[djogi::djogi_test]
async fn raw_query_decodes_typed_rows(mut ctx: djogi::DjogiContext) {
    let rows = ctx
        .raw_query::<RawQueryProbe>("SELECT $1::integer AS value", &[&41_i32])
        .await
        .expect("raw_query should decode typed rows");

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].value, 41);
}
